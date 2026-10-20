use crate::plugin::loader::wasm::wasm_host::WasmPlugin;
use crate::server::Server;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

pub type TaskId = u32;

/// HCF-CORE-025: scheduler counters for diagnosing overruns without a profiler.
#[derive(Default)]
struct TaskSchedulerMetrics {
    ticks: AtomicU64,
    due: AtomicU64,
    launched: AtomicU64,
    skipped: AtomicU64,
    overrun: AtomicU64,
    max_task_nanos: AtomicU64,
    total_task_nanos: AtomicU64,
    tick_duration_nanos: AtomicU64,
}

impl TaskSchedulerMetrics {
    fn record_task(&self, nanos: u64) {
        self.total_task_nanos
            .fetch_add(nanos, AtomicOrdering::Relaxed);
        self.max_task_nanos
            .fetch_max(nanos, AtomicOrdering::Relaxed);
    }
}

/// Clears the running flag even if the guest returns an error or panics.
struct RunningGuard(Arc<AtomicBool>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, AtomicOrdering::Release);
    }
}

pub struct ScheduledTask {
    pub id: TaskId,
    pub plugin: Arc<WasmPlugin>,
    pub handler_id: u32,
    pub next_tick: u64,
    pub period: Option<u64>,
    /// HCF-CORE-025: prevents a periodic task from overlapping itself.
    running: Arc<AtomicBool>,
}

impl PartialEq for ScheduledTask {
    fn eq(&self, other: &Self) -> bool {
        self.next_tick == other.next_tick
    }
}

impl Eq for ScheduledTask {}

impl PartialOrd for ScheduledTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order so BinaryHeap is a min-heap
        other.next_tick.cmp(&self.next_tick)
    }
}

pub struct TaskScheduler {
    tasks: Mutex<BinaryHeap<ScheduledTask>>,
    cancelled_tasks: Mutex<HashSet<TaskId>>,
    disabled_plugins: Mutex<Vec<Weak<WasmPlugin>>>,
    next_task_id: std::sync::atomic::AtomicU32,
    metrics: Arc<TaskSchedulerMetrics>,
}

impl Default for TaskScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(BinaryHeap::new()),
            cancelled_tasks: Mutex::new(HashSet::new()),
            disabled_plugins: Mutex::new(Vec::new()),
            next_task_id: std::sync::atomic::AtomicU32::new(0),
            metrics: Arc::new(TaskSchedulerMetrics::default()),
        }
    }

    pub fn schedule_delayed_task(
        &self,
        plugin: Arc<WasmPlugin>,
        handler_id: u32,
        delay: u64,
        current_tick: u64,
    ) -> TaskId {
        let id = self.next_task_id.fetch_add(1, AtomicOrdering::SeqCst);
        let mut disabled_plugins = self
            .disabled_plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Self::is_plugin_disabled(&mut disabled_plugins, &plugin) {
            return id;
        }
        let task = ScheduledTask {
            id,
            plugin,
            handler_id,
            next_tick: current_tick + delay,
            period: None,
            running: Arc::new(AtomicBool::new(false)),
        };
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
        id
    }

    pub fn schedule_repeating_task(
        &self,
        plugin: Arc<WasmPlugin>,
        handler_id: u32,
        delay: u64,
        period: u64,
        current_tick: u64,
    ) -> TaskId {
        let id = self.next_task_id.fetch_add(1, AtomicOrdering::SeqCst);
        let mut disabled_plugins = self
            .disabled_plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Self::is_plugin_disabled(&mut disabled_plugins, &plugin) {
            return id;
        }
        let task = ScheduledTask {
            id,
            plugin,
            handler_id,
            next_tick: current_tick + delay,
            period: Some(period),
            running: Arc::new(AtomicBool::new(false)),
        };
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
        id
    }

    pub fn cancel_task(&self, id: TaskId) {
        self.cancelled_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id);
    }

    pub fn disable_plugin(&self, plugin: &Arc<WasmPlugin>) {
        let mut disabled_plugins = self
            .disabled_plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !Self::is_plugin_disabled(&mut disabled_plugins, plugin) {
            disabled_plugins.push(Arc::downgrade(plugin));
        }

        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|task| !Arc::ptr_eq(&task.plugin, plugin));
    }

    fn is_plugin_disabled(
        disabled_plugins: &mut Vec<Weak<WasmPlugin>>,
        plugin: &Arc<WasmPlugin>,
    ) -> bool {
        disabled_plugins.retain(|entry| entry.strong_count() > 0);
        let plugin = Arc::downgrade(plugin);
        disabled_plugins
            .iter()
            .any(|entry| Weak::ptr_eq(entry, &plugin))
    }

    #[expect(clippy::too_many_lines)]
    pub fn tick(&self, server: &Arc<Server>) {
        let tick_started = Instant::now();
        let current_tick = server.tick_count.load(AtomicOrdering::Relaxed) as u64;
        let metrics = self.metrics.clone();
        let tick_number = metrics.ticks.fetch_add(1, AtomicOrdering::Relaxed) + 1;
        let mut tasks_to_run = Vec::new();

        {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut cancelled = self
                .cancelled_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            while let Some(task) = tasks.peek() {
                if task.next_tick > current_tick {
                    break;
                }

                let Some(task) = tasks.pop() else {
                    break;
                };
                if cancelled.remove(&task.id) {
                    continue;
                }

                tasks_to_run.push(task);
            }
        }
        metrics
            .due
            .fetch_add(tasks_to_run.len() as u64, AtomicOrdering::Relaxed);

        let guard_enabled = server.hcf_profile().scheduler_guard();
        let budget_ms = server.hcf_profile().scheduler_budget_ms();
        let mut disabled_plugins = self
            .disabled_plugins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut pending = tasks_to_run.into_iter();
        let processing_started = Instant::now();

        while let Some(mut task) = pending.next() {
            // HCF-CORE-025: stop launching for this tick if the budget is
            // exhausted. Unprocessed tasks stay due and are retried next tick.
            if budget_ms > 0 && processing_started.elapsed().as_millis() >= u128::from(budget_ms) {
                metrics.overrun.fetch_add(1, AtomicOrdering::Relaxed);
                let mut tasks = self
                    .tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                tasks.push(task);
                for task in pending {
                    tasks.push(task);
                }
                break;
            }

            if Self::is_plugin_disabled(&mut disabled_plugins, &task.plugin) {
                continue;
            }

            let running_guard = if guard_enabled && task.period.is_some() {
                let running = task.running.clone();
                if running
                    .compare_exchange(
                        false,
                        true,
                        AtomicOrdering::Acquire,
                        AtomicOrdering::Relaxed,
                    )
                    .is_err()
                {
                    metrics.skipped.fetch_add(1, AtomicOrdering::Relaxed);
                    if !Self::is_plugin_disabled(&mut disabled_plugins, &task.plugin)
                        && let Some(period) = task.period
                    {
                        task.next_tick = current_tick + period;
                        self.tasks
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(task);
                    }
                    continue;
                }
                Some(RunningGuard(running))
            } else {
                None
            };

            // Run the task
            let plugin = task.plugin.clone();
            let handler_id = task.handler_id;
            let server_clone = server.clone();
            let metrics_task = metrics.clone();
            let task_started = Instant::now();

            server.spawn_task(async move {
                let _running_guard = running_guard;
                let function = match plugin.plugin_instance.as_ref() {
                    crate::plugin::loader::wasm::wasm_host::PluginInstance::V0_1(instance) => {
                        instance.func_handle_task()
                    }
                };
                let result = plugin
                    .store
                    .call_guest(move |mut guest| {
                        Box::pin(async move {
                            let (server_resource, server_rep) = guest.with(|mut store| {
                                let resource = store.data_mut().add_server(server_clone)?;
                                let rep = resource.rep();
                                Ok::<_, wasmtime::Error>((resource, rep))
                            })?;
                            let result = guest.call(function, (handler_id, server_resource)).await;
                            guest.with(|mut store| {
                                let _ = store.data_mut().resource_table.delete::<
                                    crate::plugin::loader::wasm::wasm_host::state::ServerResource,
                                >(wasmtime::component::Resource::new_own(server_rep));
                            });
                            result
                        })
                    })
                    .await;
                metrics_task.record_task(task_started.elapsed().as_nanos() as u64);
                if let Err(error) = result {
                    tracing::error!(handler_id, %error, "Wasm scheduled task failed");
                }
            });
            metrics.launched.fetch_add(1, AtomicOrdering::Relaxed);

            // If repeating, schedule next run
            if let Some(period) = task.period {
                task.next_tick = current_tick + period;
                if !Self::is_plugin_disabled(&mut disabled_plugins, &task.plugin) {
                    self.tasks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(task);
                }
            }
        }

        metrics.tick_duration_nanos.store(
            tick_started.elapsed().as_nanos() as u64,
            AtomicOrdering::Relaxed,
        );
        if tick_number.is_multiple_of(600) {
            tracing::info!(
                "HCF_METRIC scheduler ticks={} due={} launched={} skipped={} overrun={} duration_us={} max_task_us={} total_task_us={}",
                tick_number,
                metrics.due.load(AtomicOrdering::Relaxed),
                metrics.launched.load(AtomicOrdering::Relaxed),
                metrics.skipped.load(AtomicOrdering::Relaxed),
                metrics.overrun.load(AtomicOrdering::Relaxed),
                metrics.tick_duration_nanos.load(AtomicOrdering::Relaxed) / 1_000,
                metrics.max_task_nanos.load(AtomicOrdering::Relaxed) / 1_000,
                metrics.total_task_nanos.load(AtomicOrdering::Relaxed) / 1_000,
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledFunctionEvent {
    pub id: String,
    pub trigger_tick: u64,
    pub function_name: String,
    pub is_tag: bool,
}

impl PartialOrd for ScheduledFunctionEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledFunctionEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        other.trigger_tick.cmp(&self.trigger_tick)
    }
}

#[derive(Default)]
pub struct ScheduledFunctionQueue {
    queue: Mutex<BinaryHeap<ScheduledFunctionEvent>>,
}

impl ScheduledFunctionQueue {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            queue: Mutex::new(BinaryHeap::new()),
        }
    }

    pub fn schedule(
        &self,
        id: String,
        trigger_tick: u64,
        function_name: String,
        is_tag: bool,
        replace: bool,
    ) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if replace {
            let mut retained = Vec::new();
            while let Some(event) = queue.pop() {
                if event.id != id {
                    retained.push(event);
                }
            }
            for event in retained {
                queue.push(event);
            }
        }
        queue.push(ScheduledFunctionEvent {
            id,
            trigger_tick,
            function_name,
            is_tag,
        });
    }

    pub fn remove(&self, id: &str) -> usize {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut count = 0;
        let mut retained = Vec::new();
        while let Some(event) = queue.pop() {
            if event.id == id {
                count += 1;
            } else {
                retained.push(event);
            }
        }
        for event in retained {
            queue.push(event);
        }
        count
    }

    #[must_use]
    pub fn get_event_ids(&self) -> Vec<String> {
        let queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ids: Vec<String> = queue.iter().map(|e| e.id.clone()).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    pub fn tick(&self, server: &Arc<Server>, current_tick: u64) {
        let mut to_run = Vec::new();
        {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while let Some(event) = queue.peek() {
                if event.trigger_tick > current_tick {
                    break;
                }
                if let Some(event) = queue.pop() {
                    to_run.push(event);
                }
            }
        }
        for event in to_run {
            let _ = crate::data::datapack::DatapackManager::execute_function_from_console(
                server,
                &event.function_name,
            );
        }
    }
}
