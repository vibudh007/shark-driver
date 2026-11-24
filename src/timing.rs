use std::time::Instant;

pub struct Span {
    name: &'static str,
    start: Option<Instant>,
}

impl Span {
    pub fn start(name: &'static str) -> Self {
        Self {
            name,
            start: (std::env::var_os("R1_TIMINGS").as_deref() == Some("1".as_ref()))
                .then(Instant::now),
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            eprintln!("r1-timing\t{}\t{}", self.name, start.elapsed().as_micros());
        }
    }
}
