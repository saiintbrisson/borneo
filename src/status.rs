use anyhow::Result;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::types::ArtifactCoordinates;

pub(crate) enum Status {
    Begin { key: String, msg: String },
    End(String),
    Error(String, anyhow::Error),
    Fatal(String),
    Log(String),
    Output(Vec<u8>),
}

pub(crate) static STATUS: std::sync::OnceLock<StatusHandle> = std::sync::OnceLock::new();

pub struct StatusHandle(std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<Status>>>);

impl StatusHandle {
    pub(crate) fn init() -> tokio::sync::mpsc::UnboundedReceiver<Status> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        STATUS
            .set(Self(std::sync::Mutex::new(Some(tx))))
            .ok()
            .expect("StatusHandle already initialized");
        rx
    }

    pub fn get() -> &'static Self {
        STATUS.get().expect("StatusHandle not initialized")
    }

    pub(crate) fn send(&self, status: Status) {
        if let Some(tx) = self.0.lock().unwrap().as_ref() {
            let _ = tx.send(status);
        }
    }

    pub(crate) fn shutdown(&self) {
        self.0.lock().unwrap().take();
    }

    pub fn begin(&self, key: impl Into<String>, msg: impl Into<String>) {
        self.send(Status::Begin {
            key: key.into(),
            msg: msg.into(),
        });
    }

    pub fn end(&self, key: impl Into<String>) {
        self.send(Status::End(key.into()));
    }

    pub fn resolving(&self, coord: &ArtifactCoordinates) {
        self.begin(coord.to_string(), format!("resolving {coord}"));
    }

    pub fn resolved(&self, coord: &ArtifactCoordinates) {
        self.end(coord.to_string());
    }

    pub fn downloading(&self, coord: &ArtifactCoordinates) {
        self.begin(format!("dl:{coord}"), format!("downloading {coord}"));
    }

    pub fn downloaded(&self, coord: &ArtifactCoordinates) {
        self.end(format!("dl:{coord}"));
    }

    pub fn error(&self, coord: ArtifactCoordinates, err: anyhow::Error) {
        self.send(Status::Error(coord.to_string(), err));
    }

    pub fn log(&self, msg: impl Into<String>) {
        self.send(Status::Log(msg.into()));
    }

    pub fn fatal(&self, msg: impl Into<String>) {
        self.send(Status::Fatal(msg.into()));
    }

    pub fn output(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.send(Status::Output(bytes));
        }
    }
}

const MAX_VISIBLE: usize = 4;

struct ProgressDisplay {
    multi: MultiProgress,
    slots: Vec<ProgressBar>,
    overflow_bar: ProgressBar,
    active_style: ProgressStyle,
    empty_style: ProgressStyle,
    queue: Vec<(String, String)>,
    fatal: Option<anyhow::Error>,
}

impl ProgressDisplay {
    fn new() -> Self {
        let multi = MultiProgress::new();
        let active_style = ProgressStyle::with_template("{spinner:.green} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "]);
        let empty_style = ProgressStyle::with_template("").unwrap();

        let slots: Vec<_> = (0..MAX_VISIBLE)
            .map(|_| {
                let pb = multi.add(ProgressBar::new_spinner());
                pb.set_style(empty_style.clone());
                pb
            })
            .collect();
        let overflow_bar = multi.add(ProgressBar::new_spinner());
        overflow_bar.set_style(empty_style.clone());

        Self {
            multi,
            slots,
            overflow_bar,
            active_style,
            empty_style,
            queue: Vec::new(),
            fatal: None,
        }
    }

    fn handle(&mut self, status: Status) {
        if self.fatal.is_some() {
            return;
        }
        match status {
            Status::Begin { key, msg } => self.push(key, msg),
            Status::End(key) => self.remove(&key),
            Status::Error(key, err) => {
                self.remove(&key);
                self.multi.println(format!("✗ {key}: {err}")).ok();
                self.fatal = Some(err);
            }
            Status::Fatal(msg) => {
                self.multi.println(msg).ok();
                self.fatal = Some(anyhow::anyhow!(""));
            }
            Status::Log(msg) => {
                self.multi.println(msg).ok();
            }
            Status::Output(bytes) => {
                if let Ok(s) = String::from_utf8(bytes) {
                    for line in s.lines() {
                        self.multi.println(line).ok();
                    }
                }
            }
        }
    }

    fn push(&mut self, key: String, msg: String) {
        self.queue.push((key, msg));
        self.refresh();
    }

    fn remove(&mut self, key: &str) {
        self.queue.retain(|(k, _)| k != key);
        self.refresh();
    }

    fn refresh(&self) {
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some((_, msg)) = self.queue.get(i) {
                slot.set_style(self.active_style.clone());
                slot.enable_steady_tick(std::time::Duration::from_millis(80));
                slot.set_message(msg.clone());
            } else {
                slot.set_style(self.empty_style.clone());
                slot.set_message("");
                slot.disable_steady_tick();
            }
        }
        let overflow = self.queue.len().saturating_sub(MAX_VISIBLE);
        if overflow > 0 {
            self.overflow_bar
                .set_style(ProgressStyle::with_template("  {msg}").unwrap());
            self.overflow_bar
                .set_message(format!("and {overflow} more..."));
        } else {
            self.overflow_bar.set_style(self.empty_style.clone());
            self.overflow_bar.set_message("");
        }
    }

    fn finish(self) -> Result<()> {
        for slot in &self.slots {
            slot.finish_and_clear();
        }
        self.overflow_bar.finish_and_clear();
        match self.fatal {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

pub fn spawn_progress(mut rx: UnboundedReceiver<Status>) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let mut display = ProgressDisplay::new();
        while let Some(status) = rx.recv().await {
            display.handle(status);
        }
        display.finish()
    })
}
