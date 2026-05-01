use active_call::main_builder::MainBuilder;
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    MainBuilder::main()
}
