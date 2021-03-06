//! Test RNG hardware

use hil::rng::{Client, Continue, RNG};

pub struct TestRng<'a> {
    rng: &'a RNG<'a>,
}

impl<'a> TestRng<'a> {
    pub fn new(rng: &'a RNG<'a>) -> Self {
        TestRng { rng: rng }
    }

    pub fn run(&self) {
        self.rng.get();
    }
}

impl<'a> Client for TestRng<'a> {
    fn randomness_available(&self, randomness: &mut Iterator<Item = u32>) -> Continue {
        print!("Randomness: \r");
        randomness.take(5).for_each(|r| print!("  [{:x}]\r", r));
        Continue::Done
    }
}
