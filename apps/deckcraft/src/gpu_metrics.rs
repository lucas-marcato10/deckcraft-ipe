//! GPU utilization and memory, sampled from `nvidia-smi` on a slow timer.
//!
//! Sampling is opt-in (`DECKCRAFT_GPU_METRICS=1`) and never runs per frame: `nvidia-smi` costs tens
//! of milliseconds and would become the measurement's own bottleneck. A 5 s cadence is enough to
//! see a texture-cache leak (memory) or a renderer that fell off the GPU (utilization).
//!
//! This is the only GPU signal available to us. `deckcraft.gpu.frame.time` needs timestamp queries
//! the egui fork does not expose, and inventing it from CPU timings would blame the wrong device.

/// One `nvidia-smi` reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Utilization as a fraction (0–1), because `deckcraft.gpu.utilization` is unit `1`.
    pub utilization: f64,
    /// MiB reported by the driver, in bytes (`deckcraft.gpu.memory.used` is unit `By`).
    pub memory_bytes: f64,
}

const MIB: f64 = 1024.0 * 1024.0;

/// How often the driver is asked. Slow on purpose: this is a capacity/leak signal, not a frame
/// signal, and the process spawn itself costs tens of milliseconds.
const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Read the first GPU out of `nvidia-smi`'s `csv,noheader,nounits` output.
///
/// Anything unreadable returns `None`: a fabricated zero would look exactly like an idle GPU, and
/// an idle GPU is a conclusion the report is allowed to draw from this gauge.
fn parse_sample(out: &str) -> Option<Sample> {
    let line = out.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut fields = line.split(',');
    let percent: f64 = fields.next()?.trim().parse().ok()?;
    let mib: f64 = fields.next()?.trim().parse().ok()?;
    if !percent.is_finite() || !mib.is_finite() || mib < 0.0 {
        return None;
    }
    Some(Sample { utilization: (percent / 100.0).clamp(0.0, 1.0), memory_bytes: mib * MIB })
}

/// The sampling thread, stopped on the way out so it cannot record into a shut-down provider.
pub struct Sampler {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// Start sampling when `DECKCRAFT_GPU_METRICS=1` is set. `None` otherwise — and also when the
/// thread cannot be spawned, which is not worth failing a launch over.
pub fn start() -> Option<Sampler> {
    // Opt-in: only the presence of the variable matters, not its value.
    std::env::var_os("DECKCRAFT_GPU_METRICS")?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let stop = stop.clone();
        std::thread::Builder::new().name("gpu-metrics".to_owned()).spawn(move || {
            use std::sync::atomic::Ordering;
            while !stop.load(Ordering::Relaxed) {
                sample();
                // Sleep in slices so a quit does not wait a whole period for the thread.
                for _ in 0..(SAMPLE_EVERY.as_millis() / 250) {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        })
    }
    .ok()?;
    log::info!("gpu metrics: sampling nvidia-smi every {}s", SAMPLE_EVERY.as_secs());
    Some(Sampler { stop, worker: Some(worker) })
}

impl Sampler {
    /// Ask the thread to finish and wait, but never hold up process exit on a stuck `nvidia-smi`.
    ///
    /// A detached thread holds only an `Arc<AtomicBool>` and `&'static` instrument handles, both
    /// valid until process exit, and recording into an already-shut-down provider is a no-op the
    /// SDK handles. One sleep slice is 250 ms; the grace period is twice that.
    pub fn stop(&mut self) {
        use std::sync::atomic::Ordering;
        self.stop.store(true, Ordering::Relaxed);
        let Some(worker) = self.worker.take() else { return };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while !worker.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if worker.is_finished() {
            let _ = worker.join();
        } else {
            log::warn!("gpu metrics: sampler did not stop within 500 ms; detaching");
        }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
impl Sampler {
    fn with_worker_for_test(stop: std::sync::Arc<std::sync::atomic::AtomicBool>, worker: std::thread::JoinHandle<()>) -> Sampler {
        Sampler { stop, worker: Some(worker) }
    }
}

/// One reading, recorded into the two gauges.
fn sample() {
    let output = std::process::Command::new("nvidia-smi").args(["--query-gpu=utilization.gpu,memory.used", "--format=csv,noheader,nounits"]).output();
    let Ok(output) = output else {
        // Debug, not warn: a machine without the driver would log every five seconds forever.
        log::debug!("gpu metrics: nvidia-smi is not available");
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(sample) = parse_sample(&text) else {
        log::debug!("gpu metrics: unreadable nvidia-smi output");
        return;
    };
    let instruments = deckcraft_telemetry::instruments();
    instruments.gpu_utilization.record(sample.utilization, &[]);
    instruments.gpu_memory_used.record(sample.memory_bytes, &[]);
    log::debug!("gpu: util={:.0}% mem={:.0} MiB", sample.utilization * 100.0, sample.memory_bytes / MIB);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: f64 = 1024.0 * 1024.0;

    /// The query is `--query-gpu=utilization.gpu,memory.used --format=csv,noheader,nounits`, so one
    /// line per GPU: `utilization %, memory MiB`.
    #[test]
    fn a_sample_is_percent_and_mib() {
        let sample = parse_sample("0, 543\n").expect("a real sample line must parse");
        assert_eq!(sample.utilization, 0.0);
        assert!((sample.memory_bytes - 543.0 * MIB).abs() < 1.0, "{sample:?}");
        let full = parse_sample("100, 2048").expect("a full GPU must parse");
        assert_eq!(full.utilization, 1.0);
        assert!((full.memory_bytes - 2048.0 * MIB).abs() < 1.0, "{full:?}");
    }

    /// The first GPU is the one the app renders with. A machine with several cards prints one line
    /// per card and the sampler must not average them.
    #[test]
    fn only_the_first_gpu_is_read() {
        let sample = parse_sample("34, 1024\n7, 8192\n").expect("a multi-GPU machine must parse");
        assert_eq!(sample.utilization, 0.34);
        assert!((sample.memory_bytes - 1024.0 * MIB).abs() < 1.0, "{sample:?}");
    }

    /// A driver that answers with something else (an error message, a missing field, a unit we did
    /// not ask for) yields no sample rather than a zero that looks like an idle GPU.
    #[test]
    fn an_unreadable_sample_is_not_a_zero() {
        for text in ["", "\n", "No devices were found", "5", "not, 543", "5, not", ", "] {
            assert_eq!(parse_sample(text), None, "{text:?} must not parse");
        }
    }

    /// Utilization is a fraction by contract; a driver that reports past 100 % (or a negative) must
    /// not put an impossible value into a gauge whose axis is 0–1.
    #[test]
    fn utilization_is_clamped_to_the_unit_interval() {
        assert_eq!(parse_sample("150, 10").map(|s| s.utilization), Some(1.0));
        assert_eq!(parse_sample("-5, 10").map(|s| s.utilization), Some(0.0));
    }

    /// `nvidia-smi` can hang while the driver is busy, and `stop()` runs from the app's `on_exit`:
    /// waiting on that thread without a deadline would keep the process alive forever. The worker
    /// here ignores the stop flag for two seconds — `stop()` must give up long before that.
    #[test]
    fn stop_does_not_block_on_a_stuck_worker() {
        use std::sync::{Arc, atomic::AtomicBool};
        use std::time::{Duration, Instant};

        let stop = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn(|| {
            // Simula um `nvidia-smi` travado: ignora o flag por 2 s.
            std::thread::sleep(Duration::from_secs(2));
        });
        let mut sampler = Sampler::with_worker_for_test(stop, worker);
        let t0 = Instant::now();
        sampler.stop();
        assert!(t0.elapsed() < Duration::from_millis(900), "stop() bloqueou: {:?}", t0.elapsed());
    }
}
