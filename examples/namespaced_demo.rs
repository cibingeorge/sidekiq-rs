use async_trait::async_trait;
use bb8::Pool;
use sidekiq::{Processor, RedisConnectionManager, Worker};
use tokio_shutdown::Shutdown;

#[derive(Clone)]
struct HelloWorker;

#[async_trait]
impl Worker<()> for HelloWorker {
    async fn perform(&self, _args: ()) -> Result<(), Box<dyn std::error::Error>> {
        println!("Hello, world!");

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Redis
    let manager = RedisConnectionManager::new("redis://127.0.0.1/")?;
    let redis = Pool::builder()
        .max_size(100)
        .connection_customizer(sidekiq::with_custom_namespace("yolo_app".to_string()))
        .build(manager)
        .await?;

    tokio::spawn({
        let mut redis = redis.clone();

        async move {
            loop {
                HelloWorker::perform_async(&mut redis, ()).await.unwrap();

                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    });

    let shutdown = Shutdown::new().unwrap();
    // Sidekiq server
    let mut p = Processor::new(redis.clone(), vec!["default".to_string()], shutdown);

    // Add known workers
    p.register(HelloWorker);

    // Start!
    p.run().await;
    Ok(())
}
