use serde::{Deserialize, Serialize};

/// Core DSP effect interface. Implementations live in `crates/effects`.
pub trait Effect: Send {
    fn name(&self) -> &str;
    fn process(&mut self, buffer: &mut [f32]);
    fn parameters(&self) -> Vec<EffectParam>;
    fn set_parameter(&mut self, name: &str, value: f32);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectParam {
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub current: f32,
}

/// Ordered chain of effects applied to a single virtual sink.
pub struct EffectsChain {
    effects: Vec<Box<dyn Effect>>,
}

impl EffectsChain {
    pub fn new() -> Self {
        Self { effects: Vec::new() }
    }

    pub fn push(&mut self, effect: Box<dyn Effect>) {
        self.effects.push(effect);
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.effects.len() {
            self.effects.remove(index);
        }
    }

    pub fn process(&mut self, buffer: &mut [f32]) {
        for effect in &mut self.effects {
            effect.process(buffer);
        }
    }
}

impl Default for EffectsChain {
    fn default() -> Self {
        Self::new()
    }
}
