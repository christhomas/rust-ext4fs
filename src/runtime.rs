//! Environmental values used by mounted-filesystem mutations.
//!
//! A provider can be injected for deterministic tests or an embedding runtime.
//! Formatting has its own UUID policy and is not changed by this interface.
use std::sync::atomic::{AtomicU32, Ordering};

pub trait Runtime: Send + Sync {
    /// Seconds since the Unix epoch, matching ext4's legacy timestamp fields.
    fn now_unix_seconds(&self) -> u32;
    /// New inode generation; implementations must avoid immediate reuse.
    fn next_inode_generation(&self) -> u32;
}

/// Native defaults retain the process-ID/counter and SystemTime behavior.
/// Browser builds use JavaScript wall time and a random process-equivalent seed.
pub struct SystemRuntime;
static COUNTER: AtomicU32 = AtomicU32::new(1);

impl Runtime for SystemRuntime {
    fn now_unix_seconds(&self) -> u32 {
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        {
            (js_sys::Date::now() / 1000.0) as u32
        }
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0)
        }
    }
    fn next_inode_generation(&self) -> u32 {
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        let seed = {
            // Randomize once for this WASM instance, then preserve the native
            // monotonic wrapping counter semantics within the instance.
            static SEED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
            *SEED.get_or_init(|| (js_sys::Math::random() * 4294967296.0) as u32)
        };
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        let seed = std::process::id();
        seed.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}
