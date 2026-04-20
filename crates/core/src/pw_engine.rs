use anyhow::Result;
use pipewire as pw;

/// PipeWire engine handle. The actual loop runs in a background thread.
/// All PW operations must be dispatched through the sender returned by `start()`.
pub struct PwEngine;

impl PwEngine {
    /// Initialise PipeWire and start the event loop in a background thread.
    /// Returns a `Sender` that is safe to use from any thread.
    pub fn start() -> Result<pw::channel::Sender<EngineCmd>> {
        pw::init();
        let (tx, rx) = pw::channel::channel::<EngineCmd>();

        std::thread::spawn(move || {
            // MainLoopBox must be created on the PW thread — it is not Send.
            let main_loop = pw::main_loop::MainLoopBox::new(None)
                .expect("failed to create PipeWire main loop");

            let _attached = rx.attach(main_loop.loop_(), |_cmd| {
                // TODO: handle EngineCmd variants; shutdown requires signalling the loop
            });

            main_loop.run();
        });

        Ok(tx)
    }
}

pub enum EngineCmd {
    Shutdown,
}
