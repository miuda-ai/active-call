use active_call::main_builder::MainBuilder;
use anyhow::Result;

fn main() -> Result<()> {
    // Deep async call-setup chains (accept -> track handshake) need more than
    // the default 2 MB worker stack in debug builds; 8 MB matches the main
    // thread size the test runtime uses.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?;
    runtime.block_on(MainBuilder::default().run())
}
