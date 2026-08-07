use core::ffi::{c_char, CStr};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use claw_interface::{ClawThread, CoreAffinity, Priority, WorkerHandle};
use claw_sys::EspIdfThread;
use claw_tool::{
    AsyncToolHandler, RetryCount, Tool, ToolError, ToolFuture, ToolGroup, ToolInvocation,
    ToolInvokeError, ToolOutput, ToolResult, ToolSpec,
};
use futures_channel::oneshot;
use serde_json::json;

use crate::abi::{
    claw_cap_call, claw_cap_get_descriptor_state, claw_cap_is_llm_tool_available, claw_cap_list,
    ClawCapCallContext, ClawCapDescriptor, ClawCapDescriptorInfo, CLAW_CAP_FLAG_CALLABLE_BY_LLM,
    CLAW_CAP_FLAG_ROOT_AGENT_ONLY, CLAW_CAP_KIND_CALLABLE, CLAW_CAP_KIND_HYBRID, ESP_OK,
    TOOL_OUTPUT_CAPACITY,
};

const CAPABILITY_EXECUTOR_STACK_SIZE: usize = 32 * 1024;
const CAPABILITY_EXECUTOR_QUEUE_CAPACITY: usize = 8;

type CapabilityJob = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CapToolError {
    #[error("invalid capability registry list")]
    InvalidList,
    #[error("invalid capability descriptor")]
    InvalidDescriptor,
    #[error("invalid capability schema: {0}")]
    InvalidSchema(String),
    #[error("failed to start capability executor: {0}")]
    ExecutorSpawn(#[source] std::io::Error),
}

pub(crate) fn capability_tool_groups(
    filtered_group_ids: &[&str],
    executor: CapabilityExecutor,
) -> Result<Vec<ToolGroup>, CapToolError> {
    let list = unsafe { claw_cap_list() };
    if list.count > 0 && list.items.is_null() {
        return Err(CapToolError::InvalidList);
    }

    let mut groups = BTreeMap::<String, Vec<Tool>>::new();
    for index in 0..list.count {
        let descriptor =
            unsafe { list.items.add(index).as_ref() }.ok_or(CapToolError::InvalidDescriptor)?;
        if !is_llm_tool(descriptor) {
            continue;
        }
        let group_id = descriptor_group_id(descriptor)?;
        if filtered_group_ids.contains(&group_id.as_str()) {
            continue;
        }
        if !is_available_to_root_agent(descriptor)? {
            continue;
        }
        groups
            .entry(group_id)
            .or_default()
            .push(Tool::from_async(CapTool::try_from(
                descriptor,
                executor.clone(),
            )?));
    }
    Ok(groups
        .into_iter()
        .map(|(group_id, tools)| ToolGroup::new(group_id, false, tools))
        .collect())
}

fn is_available_to_root_agent(descriptor: &ClawCapDescriptor) -> Result<bool, CapToolError> {
    let name = c_string(descriptor.name)
        .or_else(|| c_string(descriptor.id))
        .ok_or(CapToolError::InvalidDescriptor)?;
    let c_name = CString::new(name).map_err(|_| CapToolError::InvalidDescriptor)?;
    let ctx = ClawCapCallContext::default();
    Ok(unsafe { claw_cap_is_llm_tool_available(c_name.as_ptr(), &ctx) })
}

fn is_llm_tool(descriptor: &ClawCapDescriptor) -> bool {
    matches!(
        descriptor.kind,
        CLAW_CAP_KIND_CALLABLE | CLAW_CAP_KIND_HYBRID
    ) && descriptor.execute.is_some()
        && descriptor.cap_flags & CLAW_CAP_FLAG_CALLABLE_BY_LLM != 0
        && descriptor.cap_flags & CLAW_CAP_FLAG_ROOT_AGENT_ONLY == 0
}

fn descriptor_group_id(descriptor: &ClawCapDescriptor) -> Result<String, CapToolError> {
    let name = c_string(descriptor.name)
        .or_else(|| c_string(descriptor.id))
        .ok_or(CapToolError::InvalidDescriptor)?;
    let c_name = CString::new(name).map_err(|_| CapToolError::InvalidDescriptor)?;
    let mut info = ClawCapDescriptorInfo {
        id: core::ptr::null(),
        name: core::ptr::null(),
        group_id: core::ptr::null(),
        state: 0,
        active_calls: 0,
    };
    let err = unsafe { claw_cap_get_descriptor_state(c_name.as_ptr(), &mut info) };
    if err != ESP_OK {
        return Err(CapToolError::InvalidDescriptor);
    }
    c_string(info.group_id)
        .filter(|group_id| !group_id.is_empty())
        .ok_or(CapToolError::InvalidDescriptor)
}

/// Runs the synchronous C capability ABI away from the cooperative Session
/// executor. Tool futures enqueue owned calls and await a one-shot result, so
/// polling one capability can never park every Session/Agent behind a blocking
/// `execute` implementation such as `esp_http_client_perform`.
#[derive(Clone)]
pub(crate) struct CapabilityExecutor {
    inner: Arc<CapabilityExecutorInner>,
}

struct CapabilityExecutorInner {
    sender: mpsc::SyncSender<CapabilityJob>,
    stopping: Arc<AtomicBool>,
    worker: Mutex<Option<WorkerHandle>>,
}

impl CapabilityExecutor {
    pub(crate) fn new() -> Result<Self, CapToolError> {
        let (sender, receiver) =
            mpsc::sync_channel::<CapabilityJob>(CAPABILITY_EXECUTOR_QUEUE_CAPACITY);
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = Arc::clone(&stopping);
        let worker = EspIdfThread::spawn_worker(
            "claw_cap_exec",
            CAPABILITY_EXECUTOR_STACK_SIZE,
            Priority::Low,
            CoreAffinity::Any,
            move || {
                log::info!("Capability executor started");
                while let Ok(job) = receiver.recv() {
                    if worker_stopping.load(Ordering::Acquire) {
                        break;
                    }
                    job();
                }
                log::info!("Capability executor stopped");
            },
        )
        .map_err(CapToolError::ExecutorSpawn)?;

        Ok(Self {
            inner: Arc::new(CapabilityExecutorInner {
                sender,
                stopping,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }

    fn submit(&self, name: String, arguments_json: String) -> ToolFuture<'static> {
        let executor = self.clone();
        Box::pin(async move {
            let (sender, receiver) = oneshot::channel();
            let capability_name = name.clone();
            let job: CapabilityJob = Box::new(move || {
                if sender.is_canceled() {
                    log::info!("Capability call skipped after cancellation: {capability_name}");
                    return;
                }
                let started = Instant::now();
                log::info!("Capability call started: {capability_name}");
                let result = call_capability(&capability_name, &arguments_json);
                log::info!(
                    "Capability call finished: {capability_name} ok={} elapsed_ms={}",
                    result.is_ok(),
                    started.elapsed().as_millis()
                );
                let _ = sender.send(result);
            });

            executor
                .inner
                .sender
                .try_send(job)
                .map_err(|error| match error {
                    mpsc::TrySendError::Full(_) => ToolError::InvokeRejected(format!(
                        "capability executor queue is full for {name}"
                    )),
                    mpsc::TrySendError::Disconnected(_) => ToolError::InvokeRejected(format!(
                        "capability executor is not available for {name}"
                    )),
                })?;

            match receiver.await {
                Ok(result) => result,
                Err(_) => Err(ToolError::InvokeRejected(format!(
                    "capability executor exited before {name} completed"
                ))
                .into()),
            }
        })
    }
}

impl Drop for CapabilityExecutorInner {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        // Wake a worker blocked on `recv`; it observes `stopping` before
        // executing this no-op or any call still queued behind the active one.
        let _ = self.sender.try_send(Box::new(|| {}));
        if let Ok(worker) = self.worker.get_mut() {
            if let Some(worker) = worker.take() {
                worker.join();
            }
        }
    }
}

struct CapTool {
    name: String,
    schema: String,
    usage: Option<String>,
    executor: CapabilityExecutor,
}

impl CapTool {
    fn try_from(
        descriptor: &ClawCapDescriptor,
        executor: CapabilityExecutor,
    ) -> Result<Self, CapToolError> {
        let name = c_string(descriptor.name)
            .or_else(|| c_string(descriptor.id))
            .ok_or(CapToolError::InvalidDescriptor)?;
        let input_schema =
            c_string(descriptor.input_schema_json).ok_or(CapToolError::InvalidDescriptor)?;
        let description = c_string(descriptor.description);
        let description_text = description.as_deref().unwrap_or_default();

        let parameters = serde_json::from_str::<serde_json::Value>(&input_schema)
            .map_err(|error| CapToolError::InvalidSchema(error.to_string()))?;
        let schema = json!({
            "type": "function",
            "function": {
                "name": &name,
                "description": description_text,
                "parameters": parameters,
            }
        })
        .to_string();

        Ok(Self {
            name,
            schema,
            usage: description,
            executor,
        })
    }
}

impl ToolSpec for CapTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> &str {
        &self.schema
    }

    fn usage(&self) -> Option<&str> {
        self.usage.as_deref()
    }

    fn retry_count(&self) -> RetryCount {
        RetryCount::none()
    }
}

impl AsyncToolHandler for CapTool {
    fn invoke<'a>(&'a self, call: &'a ToolInvocation) -> ToolFuture<'a> {
        if call.name() != self.name {
            let name = call.name().to_owned();
            return Box::pin(async move { Err(ToolError::NotFound(name).into()) });
        }
        self.executor
            .submit(self.name.clone(), call.arguments_json().to_owned())
    }
}

pub(crate) fn call_capability(name: &str, arguments_json: &str) -> ToolResult<ToolOutput> {
    let name = cstring(name)?;
    let arguments_json = cstring(arguments_json)?;
    let mut output = vec![0u8; TOOL_OUTPUT_CAPACITY];
    let ctx = ClawCapCallContext::default();
    let err = unsafe {
        claw_cap_call(
            name.as_ptr(),
            arguments_json.as_ptr(),
            &ctx,
            output.as_mut_ptr().cast::<c_char>(),
            output.len(),
        )
    };
    let output = c_buffer_to_string(&output);
    if err == ESP_OK {
        Ok(ToolOutput {
            content: output,
            ok: true,
        })
    } else {
        Err(ToolError::InvokeRejected(output).into())
    }
}

fn c_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr).to_str().ok().map(str::to_owned) }
}

fn cstring(value: &str) -> Result<CString, ToolInvokeError> {
    CString::new(value)
        .map_err(|_| ToolError::InvalidArguments("string contains nul".into()).into())
}

fn c_buffer_to_string(buffer: &[u8]) -> String {
    let len = match buffer.iter().position(|byte| *byte == 0) {
        Some(len) => len,
        None => buffer.len(),
    };
    let payload = match buffer.get(..len) {
        Some(payload) => payload,
        None => buffer,
    };
    String::from_utf8_lossy(payload).into_owned()
}
