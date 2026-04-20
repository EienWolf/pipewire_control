use crate::{Effect, EffectParam};

/// Single-band parametric EQ using a biquad filter.
pub struct ParametricEq {
    pub frequency: f32,
    pub gain_db: f32,
    pub q: f32,
    sample_rate: f32,
    // Biquad coefficients
    b0: f32, b1: f32, b2: f32, a1: f32, a2: f32,
    // Delay elements
    x1: f32, x2: f32, y1: f32, y2: f32,
}

impl ParametricEq {
    pub fn new(frequency: f32, gain_db: f32, q: f32, sample_rate: f32) -> Self {
        let mut eq = Self {
            frequency, gain_db, q, sample_rate,
            b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0,
            x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0,
        };
        eq.update_coefficients();
        eq
    }

    fn update_coefficients(&mut self) {
        let a = 10f32.powf(self.gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * self.frequency / self.sample_rate;
        let (sin_w0, cos_w0) = (w0.sin(), w0.cos());
        let alpha = sin_w0 / (2.0 * self.q);

        self.b0 = 1.0 + alpha * a;
        self.b1 = -2.0 * cos_w0;
        self.b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        self.a1 = -2.0 * cos_w0;
        self.a2 = 1.0 - alpha / a;

        // Normalise by a0
        self.b0 /= a0;
        self.b1 /= a0;
        self.b2 /= a0;
        self.a1 /= a0;
        self.a2 /= a0;
    }
}

impl Effect for ParametricEq {
    fn name(&self) -> &str {
        "Parametric EQ"
    }

    fn process(&mut self, buffer: &mut [f32]) {
        for sample in buffer.iter_mut() {
            let x = *sample;
            let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
                - self.a1 * self.y1 - self.a2 * self.y2;
            self.x2 = self.x1;
            self.x1 = x;
            self.y2 = self.y1;
            self.y1 = y;
            *sample = y;
        }
    }

    fn parameters(&self) -> Vec<EffectParam> {
        vec![
            EffectParam { name: "frequency".into(), min: 20.0, max: 20000.0, default: 1000.0, current: self.frequency },
            EffectParam { name: "gain_db".into(), min: -24.0, max: 24.0, default: 0.0, current: self.gain_db },
            EffectParam { name: "q".into(), min: 0.1, max: 10.0, default: 1.0, current: self.q },
        ]
    }

    fn set_parameter(&mut self, name: &str, value: f32) {
        match name {
            "frequency" => self.frequency = value,
            "gain_db" => self.gain_db = value,
            "q" => self.q = value,
            _ => {}
        }
        self.update_coefficients();
    }
}
