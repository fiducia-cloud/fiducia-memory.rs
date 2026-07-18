#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry = fiducia_telemetry::init("fiducia-memory");
    fiducia_memory::service::run().await
}
