use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    my_sqweel::run_cli().await
}
