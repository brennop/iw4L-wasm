use core::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct TransportBudget {
    pub response: Duration,
    pub progress: Duration,
    pub transfer: Duration,
    pub queue_age: Duration,
    pub connect: Duration,
    pub shutdown: Duration,
    pub loading: Duration,
    pub silence: Duration,
    pub admission: Duration,
    pub queue_items: usize,
    pub queue_bytes: usize,
}

impl Default for TransportBudget {
    fn default() -> Self {
        Self {
            response: Duration::from_secs(20),
            progress: Duration::from_secs(5),
            transfer: Duration::from_secs(90),
            queue_age: Duration::from_secs(20),
            connect: Duration::from_secs(60),
            shutdown: Duration::from_millis(500),
            loading: Duration::from_secs(120),
            silence: Duration::from_secs(60),
            admission: Duration::from_secs(60),
            queue_items: 32,
            queue_bytes: 8 * 1024 * 1024,
        }
    }
}

impl TransportBudget {
    pub const SETTINGS: [&'static str; 11] = [
        "IW4L_NET_RESPONSE_MS",
        "IW4L_NET_PROGRESS_MS",
        "IW4L_NET_TRANSFER_MS",
        "IW4L_NET_QUEUE_AGE_MS",
        "IW4L_NET_CONNECT_MS",
        "IW4L_NET_SHUTDOWN_MS",
        "IW4L_NET_LOADING_MS",
        "IW4L_NET_SILENCE_MS",
        "IW4L_NET_ADMISSION_MS",
        "IW4L_NET_QUEUE_ITEMS",
        "IW4L_NET_QUEUE_BYTES",
    ];

    pub fn from_settings(
        settings: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, String> {
        let mut budget = Self::default();
        for (name, text) in settings {
            let n = text
                .parse::<u64>()
                .map_err(|_| format!("{name} must be a positive integer"))?;
            match name.as_str() {
                "IW4L_NET_QUEUE_ITEMS" if (1..=1024).contains(&n) => {
                    budget.queue_items = n as usize
                }
                "IW4L_NET_QUEUE_BYTES" if (256 * 1024..=64 * 1024 * 1024).contains(&n) => {
                    budget.queue_bytes = n as usize
                }
                "IW4L_NET_RESPONSE_MS"
                | "IW4L_NET_PROGRESS_MS"
                | "IW4L_NET_TRANSFER_MS"
                | "IW4L_NET_QUEUE_AGE_MS"
                | "IW4L_NET_CONNECT_MS"
                | "IW4L_NET_SHUTDOWN_MS"
                | "IW4L_NET_LOADING_MS"
                | "IW4L_NET_SILENCE_MS"
                | "IW4L_NET_ADMISSION_MS"
                    if (1..=600_000).contains(&n) =>
                {
                    let duration = Duration::from_millis(n);
                    match name.as_str() {
                        "IW4L_NET_RESPONSE_MS" => budget.response = duration,
                        "IW4L_NET_PROGRESS_MS" => budget.progress = duration,
                        "IW4L_NET_TRANSFER_MS" => budget.transfer = duration,
                        "IW4L_NET_CONNECT_MS" => budget.connect = duration,
                        "IW4L_NET_SHUTDOWN_MS" => budget.shutdown = duration,
                        "IW4L_NET_LOADING_MS" => budget.loading = duration,
                        "IW4L_NET_SILENCE_MS" => budget.silence = duration,
                        "IW4L_NET_ADMISSION_MS" => budget.admission = duration,
                        _ => budget.queue_age = duration,
                    }
                }
                _ => return Err(format!("invalid transport setting {name}={text}")),
            }
        }
        if budget.progress > budget.transfer {
            return Err("transfer progress deadline exceeds total transfer deadline".into());
        }
        Ok(budget)
    }
}
