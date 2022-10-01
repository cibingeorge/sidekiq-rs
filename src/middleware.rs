use crate::{Counter, Job, RedisPool, UnitOfWork, WorkerRef};
use async_trait::async_trait;
use tracing::error;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type ServerResult = Result<(), Box<dyn std::error::Error>>;

#[async_trait]
pub trait ServerMiddleware {
    async fn call(
        &self,
        iter: ChainIter,
        job: &Job,
        worker: Arc<WorkerRef>,
        redis: RedisPool,
    ) -> ServerResult;
}

/// A pseudo iterator used to know which middleware should be called next.
/// This is created by the Chain type.
#[derive(Clone)]
pub struct ChainIter {
    stack: Arc<RwLock<Vec<Box<dyn ServerMiddleware + Send + Sync>>>>,
    index: usize,
}

impl ChainIter {
    #[inline]
    pub async fn next(&self, job: &Job, worker: Arc<WorkerRef>, redis: RedisPool) -> ServerResult {
        let stack = self.stack.read().await;

        if let Some(ref middleware) = stack.get(self.index) {
            middleware
                .call(
                    ChainIter {
                        stack: self.stack.clone(),
                        index: self.index + 1,
                    },
                    job,
                    worker,
                    redis,
                )
                .await?;
        }

        Ok(())
    }
}

/// A chain of middlewares that will be called in order by different server middlewares.
#[derive(Clone)]
pub(crate) struct Chain {
    stack: Arc<RwLock<Vec<Box<dyn ServerMiddleware + Send + Sync>>>>,
}

impl Chain {
    // Testing helper to get an empty chain.
    #[allow(dead_code)]
    pub(crate) fn empty() -> Self {
        Self {
            stack: Arc::new(RwLock::new(vec![])),
        }
    }

    pub(crate) fn new_with_stats(counter: Counter) -> Self {
        Self {
            stack: Arc::new(RwLock::new(vec![
                Box::new(RetryMiddleware::new()),
                Box::new(StatsMiddleware::new(counter)),
                Box::new(HandlerMiddleware),
            ])),
        }
    }

    pub(crate) async fn using(&mut self, middleware: Box<dyn ServerMiddleware + Send + Sync>) {
        let mut stack = self.stack.write().await;
        // HACK: Insert after retry middleware but before the handler middleware.
        let index = if stack.is_empty() { 0 } else { stack.len() - 1 };

        stack.insert(index, middleware);
    }

    #[inline]
    pub(crate) fn iter(&self) -> ChainIter {
        ChainIter {
            stack: self.stack.clone(),
            index: 0,
        }
    }

    #[inline]
    pub(crate) async fn call(
        &mut self,
        job: &Job,
        worker: Arc<WorkerRef>,
        redis: RedisPool,
    ) -> ServerResult {
        // The middleware must call bottom of the stack to the top.
        // Each middleware should receive a lambda to the next middleware
        // up the stack. Each middleware can short-circuit the stack by
        // not calling the "next" middleware.
        self.iter().next(job, worker, redis).await
    }
}

pub struct StatsMiddleware {
    busy_count: Counter,
}

impl StatsMiddleware {
    fn new(busy_count: Counter) -> Self {
        Self { busy_count }
    }
}

#[async_trait]
impl ServerMiddleware for StatsMiddleware {
    #[inline]
    async fn call(
        &self,
        chain: ChainIter,
        job: &Job,
        worker: Arc<WorkerRef>,
        redis: RedisPool,
    ) -> ServerResult {
        self.busy_count.incrby(1);
        let res = chain.next(job, worker, redis).await;
        self.busy_count.decrby(1);
        res
    }
}

struct HandlerMiddleware;

#[async_trait]
impl ServerMiddleware for HandlerMiddleware {
    #[inline]
    async fn call(
        &self,
        _chain: ChainIter,
        job: &Job,
        worker: Arc<WorkerRef>,
        _redis: RedisPool,
    ) -> ServerResult {
        worker.call(job.args.clone()).await
    }
}

struct RetryMiddleware {
}

impl RetryMiddleware {
    fn new() -> Self {
        Self { }
    }
}

#[async_trait]
impl ServerMiddleware for RetryMiddleware {
    #[inline]
    async fn call(
        &self,
        chain: ChainIter,
        job: &Job,
        worker: Arc<WorkerRef>,
        mut redis: RedisPool,
    ) -> ServerResult {
        let max_retries = worker.max_retries();

        let err = {
            match chain.next(job, worker, redis.clone()).await {
                Ok(()) => return Ok(()),
                Err(err) => format!("{err:?}"),
            }
        };

        let mut job = job.clone();

        // Update error fields on the job.
        job.error_message = Some(err);
        if job.retry_count.is_some() {
            job.retried_at = Some(chrono::Utc::now().timestamp() as f64);
        } else {
            job.failed_at = Some(chrono::Utc::now().timestamp() as f64);
        }
        let retry_count = job.retry_count.unwrap_or(0) + 1;
        job.retry_count = Some(retry_count);

        // Attempt the retry.
        if retry_count < max_retries {
            error!(
                status = "fail",
                class = job.class,
                jid = job.jid,
                queue = job.queue,
                err = job.error_message,
                "Scheduling job for retry in the future",
            );

            UnitOfWork::from_job(job).reenqueue(&mut redis).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{RedisConnectionManager, RedisPool, Worker};
    use bb8::Pool;
    use tokio::sync::Mutex;

    async fn redis() -> RedisPool {
        let manager = RedisConnectionManager::new("redis://127.0.0.1/").unwrap();
        Pool::builder().build(manager).await.unwrap()
    }

    fn job() -> Job {
        Job {
            class: "TestWorker".into(),
            queue: "default".into(),
            args: vec![1337].into(),
            retry: true,
            jid: crate::new_jid(),
            created_at: 1337.0,
            enqueued_at: None,
            failed_at: None,
            error_message: None,
            retry_count: None,
            retried_at: None,
        }
    }

    #[derive(Clone)]
    struct TestWorker {
        touched: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl Worker<()> for TestWorker {
        async fn perform(&self, _args: ()) -> Result<(), Box<dyn std::error::Error>> {
            *self.touched.lock().await = true;
            Ok(())
        }
    }

    #[tokio::test]
    async fn calls_through_a_middleware_stack() {
        let inner = Arc::new(TestWorker {
            touched: Arc::new(Mutex::new(false)),
        });
        let worker = Arc::new(WorkerRef::wrap(Arc::clone(&inner)));

        let job = job();
        let mut chain = Chain::empty();
        chain.using(Box::new(HandlerMiddleware)).await;
        chain
            .call(&job, worker.clone(), redis().await)
            .await
            .unwrap();

        assert!(
            *inner.touched.lock().await,
            "The job was processed by the middleware",
        );
    }
}
