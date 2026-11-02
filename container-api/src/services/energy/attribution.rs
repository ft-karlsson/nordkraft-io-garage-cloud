//! Splitting a node's measured watts between the containers on it.
//!
//! A plug measures the box, not the container, so the per-container number
//! is an attributed share and the CLI says so. The split has two parts:
//!
//! ```text
//! idle_w      = what the node draws with no containers (nodes.power_idle_watts)
//! dynamic_w   = node_w − idle_w
//! container_w = idle_w × vcpu_i / Σ vcpu  +  dynamic_w × cpu_time_i / Σ cpu_time
//! ```
//!
//! The idle part is split by reserved vCPU, so a sleeping container still
//! carries its slice of the box being on. The dynamic part follows the CPU
//! time actually used. Without a baseline everything is split by CPU time,
//! and when nothing used any CPU the split falls back to vCPU. The shares
//! always add up to the node's watts, so nothing is invented or lost.

/// One container's claim on the node.
#[derive(Debug, Clone)]
pub struct Share<'a> {
    /// Who this is, carried along for logging; the split is positional.
    #[allow(dead_code)]
    pub key: &'a str,
    /// CPU time used in the interval, microseconds.
    pub cpu_usec: u64,
    /// Reserved vCPU (the deploy's `--cpus`); 0 or less counts as 1.
    pub vcpu: f64,
}

/// Baseline known: idle split by vCPU, the rest by CPU time.
pub const METHOD_BASELINE: &str = "idle-by-vcpu,dynamic-by-cputime";
/// Baseline unknown: everything split by CPU time.
pub const METHOD_CPUTIME: &str = "all-by-cputime";

pub fn method(idle_watts: Option<f64>) -> &'static str {
    if idle_watts.is_some() {
        METHOD_BASELINE
    } else {
        METHOD_CPUTIME
    }
}

/// Watts per share, in the order given. Empty in, empty out.
pub fn attribute(node_watts: f64, idle_watts: Option<f64>, shares: &[Share<'_>]) -> Vec<f64> {
    if shares.is_empty() {
        return Vec::new();
    }
    let node_watts = node_watts.max(0.0);
    let idle = idle_watts.map(|i| i.clamp(0.0, node_watts)).unwrap_or(0.0);
    let dynamic = node_watts - idle;

    let vcpu: Vec<f64> = shares
        .iter()
        .map(|s| if s.vcpu > 0.0 { s.vcpu } else { 1.0 })
        .collect();
    let vcpu_sum: f64 = vcpu.iter().sum();
    let cpu_sum: f64 = shares.iter().map(|s| s.cpu_usec as f64).sum();

    shares
        .iter()
        .zip(vcpu.iter())
        .map(|(s, v)| {
            let static_part = idle * v / vcpu_sum;
            let dynamic_part = if cpu_sum > 0.0 {
                dynamic * s.cpu_usec as f64 / cpu_sum
            } else {
                dynamic * v / vcpu_sum
            };
            static_part + dynamic_part
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn shares_add_up_to_the_node() {
        let shares = [
            Share {
                key: "a",
                cpu_usec: 30_000_000,
                vcpu: 1.0,
            },
            Share {
                key: "b",
                cpu_usec: 10_000_000,
                vcpu: 0.5,
            },
            Share {
                key: "c",
                cpu_usec: 0,
                vcpu: 2.0,
            },
        ];
        let w = attribute(40.0, Some(20.0), &shares);
        assert!(close(w.iter().sum::<f64>(), 40.0));
        // idle 20 W by vcpu (1 : 0.5 : 2 of 3.5), dynamic 20 W by cpu (3 : 1 : 0)
        assert!(close(w[0], 20.0 * 1.0 / 3.5 + 15.0));
        assert!(close(w[1], 20.0 * 0.5 / 3.5 + 5.0));
        assert!(close(w[2], 20.0 * 2.0 / 3.5));
    }

    #[test]
    fn no_baseline_means_all_by_cpu_time() {
        let shares = [
            Share {
                key: "a",
                cpu_usec: 75,
                vcpu: 1.0,
            },
            Share {
                key: "b",
                cpu_usec: 25,
                vcpu: 4.0,
            },
        ];
        let w = attribute(10.0, None, &shares);
        assert!(close(w[0], 7.5));
        assert!(close(w[1], 2.5));
        assert_eq!(method(None), METHOD_CPUTIME);
        assert_eq!(method(Some(1.0)), METHOD_BASELINE);
    }

    #[test]
    fn nothing_busy_splits_by_vcpu() {
        let shares = [
            Share {
                key: "a",
                cpu_usec: 0,
                vcpu: 1.0,
            },
            Share {
                key: "b",
                cpu_usec: 0,
                vcpu: 3.0,
            },
        ];
        let w = attribute(8.0, Some(2.0), &shares);
        assert!(close(w[0], 2.0));
        assert!(close(w[1], 6.0));
    }

    #[test]
    fn baseline_above_the_reading_is_clamped() {
        let shares = [
            Share {
                key: "a",
                cpu_usec: 5,
                vcpu: 1.0,
            },
            Share {
                key: "b",
                cpu_usec: 5,
                vcpu: 1.0,
            },
        ];
        let w = attribute(6.0, Some(30.0), &shares);
        assert!(close(w[0], 3.0));
        assert!(close(w[1], 3.0));
        assert!(attribute(6.0, Some(3.0), &[]).is_empty());
    }

    #[test]
    fn zero_vcpu_counts_as_one() {
        let shares = [
            Share {
                key: "a",
                cpu_usec: 0,
                vcpu: 0.0,
            },
            Share {
                key: "b",
                cpu_usec: 0,
                vcpu: 1.0,
            },
        ];
        let w = attribute(4.0, Some(4.0), &shares);
        assert!(close(w[0], 2.0));
    }
}
