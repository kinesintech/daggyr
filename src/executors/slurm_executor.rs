use super::{local_executor, Result};
use crate::prelude::*;
use chrono::{DateTime, Utc};
use futures::stream::futures_unordered::FuturesUnordered;
use local_executor::expand_task_details;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, Duration};

// Traits
use futures::StreamExt;

fn default_cpus() -> usize {
    1usize
}

fn default_min_memory_mb() -> usize {
    200usize
}
fn default_min_tmp_disk_mb() -> usize {
    0usize
}
fn default_time_limit_seconds() -> usize {
    3600usize
}

fn default_priority() -> usize {
    1usize
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SlurmTaskDetail {
    pub user: String,

    pub jwt_token: String,

    #[serde(default = "default_cpus")]
    pub min_cpus: usize,

    #[serde(default = "default_min_memory_mb")]
    pub min_memory_mb: usize,

    #[serde(default = "default_min_tmp_disk_mb")]
    pub min_tmp_disk_mb: usize,

    #[serde(default = "default_priority")]
    pub priority: usize,

    #[serde(default)]
    pub time_limit_seconds: usize,

    /// The command and all arguments to run
    pub command: Vec<String>,

    /// Environment variables to set
    #[serde(default)]
    pub environment: HashMap<String, String>,

    /// Log directory. If this is readable from the server daggyr runs on,
    /// output will be read and stored in the output field of any TaskAttempts
    pub logdir: PathBuf,
}

#[derive(Serialize, Clone, Debug)]
struct SlurmSubmitJobDetails {
    name: String,
    nodes: usize,
    environment: HashMap<String, String>,
    standard_output: String,
    standard_error: String,
}

#[derive(Serialize, Clone, Debug)]
struct SlurmSubmitJob {
    script: String,
    job: SlurmSubmitJobDetails,
}

impl SlurmSubmitJob {
    fn new(task_name: String, detail: &SlurmTaskDetail) -> Self {
        let script = format!("#!/bin/bash\n{}\n", detail.command.join(" "));

        // ENV always has to have at least one value in it, so might as
        // well give it some helpful defaults
        let mut env = detail.environment.clone();
        env.insert("DAGGY_TASK_NAME".to_owned(), task_name.clone());

        let mut stdout = detail.logdir.clone();
        stdout.push(format!("{}.stdout", task_name));
        let mut stderr = detail.logdir.clone();
        stderr.push(format!("{}.stderr", task_name));

        SlurmSubmitJob {
            script,
            job: SlurmSubmitJobDetails {
                nodes: 1,
                environment: env,
                name: task_name,
                standard_output: stdout.into_os_string().into_string().unwrap(),
                standard_error: stderr.into_os_string().into_string().unwrap(),
            },
        }
    }
}

fn extract_details(details: &TaskDetails) -> Result<SlurmTaskDetail, serde_json::Error> {
    serde_json::from_value::<SlurmTaskDetail>(details.clone())
}

/// Contains the information required to monitor and resubmit failed
/// tasks. Resubmission only happens if there was a failure in the
/// cluster.
#[derive(Clone, Debug)]
struct SlurmJob {
    start_time: DateTime<Utc>,
    slurm_id: u64,
    user: String,
    jwt_token: String,
    response: mpsc::UnboundedSender<RunnerMessage>,
    run_id: RunID,
    details: TaskDetails,
    task_name: String,
    killed: bool,
}

/// Submit a task to slurmrestd, and extract the slurm `job_id`
async fn submit_slurm_job(
    base_url: &str,
    client: &reqwest::Client,
    task_id: &TaskID,
    details: &TaskDetails,
) -> Result<u64> {
    let parsed = extract_details(details).unwrap();

    let job = SlurmSubmitJob::new(task_id.to_string(), &parsed);

    let result = client
        .post(base_url.to_owned() + "/job/submit")
        .header("X-SLURM-USER-NAME", parsed.user.clone())
        .header("X-SLURM-USER-TOKEN", parsed.jwt_token.clone())
        .json(&job)
        .send()
        .await?;

    if result.status() == reqwest::StatusCode::OK {
        let payload: serde_json::Value = result.json().await.unwrap();
        Ok(payload["job_id"].as_u64().unwrap())
    } else {
        let payload: serde_json::Value = result.json().await.unwrap();
        let errors: Vec<String> = payload["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect();
        Err(anyhow!(errors.join("\n")))
    }
}

fn slurp_if_exists(filename: String) -> String {
    let pth = std::path::Path::new(&filename);
    if pth.exists() {
        std::fs::read_to_string(pth).unwrap()
    } else {
        filename
    }
}

enum JobEvent {
    Kill,
    Timeout,
}

async fn watch_job(
    slurm_id: u64,
    run_id: RunID,
    task_id: TaskID,
    details: TaskDetails,
    base_url: String,
    response: mpsc::UnboundedSender<RunnerMessage>,
    kill_signal: oneshot::Receiver<JobEvent>,
) {
    let start_time = Utc::now();
    let client = reqwest::Client::new();
    let parsed = extract_details(&details).unwrap();
    let mut signals = FuturesUnordered::new();
    signals.push(kill_signal);
    let mut killed = false;

    loop {
        // Generate a timeout for the next poll
        let (timeout_tx, timeout_rx) = oneshot::channel();
        tokio::spawn(async move {
            sleep(Duration::from_secs(1)).await;
            timeout_tx.send(JobEvent::Timeout).unwrap_or(());
        });

        signals.push(timeout_rx);

        if let Some(Ok(event)) = signals.next().await {
            match event {
                JobEvent::Kill => {
                    let url = format!("{}/job/{}", base_url, slurm_id);
                    let response = client
                        .delete(url)
                        .header("X-SLURM-USER-NAME", parsed.user.clone())
                        .header("X-SLURM-USER-TOKEN", parsed.jwt_token.clone())
                        .send()
                        .await
                        .unwrap();
                    if response.status() == 200 {
                        killed = true;
                    }
                }
                JobEvent::Timeout => {
                    let url = format!("{}/job/{}", base_url, slurm_id);
                    let result = client
                        .get(url)
                        .header("X-SLURM-USER-NAME", parsed.user.clone())
                        .header("X-SLURM-USER-TOKEN", parsed.jwt_token.clone())
                        .send()
                        .await
                        .unwrap();

                    if result.status() != 200 {
                        let error = format!(
                                    "Unable to query job status, assuming critical failure. Investigate job id {}, task name {} in slurm for more details"
                                    , slurm_id, task_id
                                );
                        response
                            .send(RunnerMessage::ExecutionReport {
                                run_id,
                                task_id,
                                attempt: TaskAttempt {
                                    executor: vec![error],
                                    ..TaskAttempt::default()
                                },
                            })
                            .unwrap();
                        return;
                    }

                    let payload: serde_json::Value = result.json().await.unwrap();
                    let job = &payload["jobs"].as_array().unwrap()[0];
                    let state = job["job_state"].as_str().unwrap();
                    match state {
                        // Completed
                        "COMPLETED" | "FAILED" | "CANCELLED" | "TIMEOUT" | "OOM" => {
                            // Attempt to read the standard out / error
                            let stderr = slurp_if_exists(
                                job["standard_error"].as_str().unwrap().to_string(),
                            );
                            let stdout = slurp_if_exists(
                                job["standard_output"].as_str().unwrap().to_string(),
                            );

                            response
                                .send(RunnerMessage::ExecutionReport {
                                    run_id,
                                    task_id,
                                    attempt: TaskAttempt {
                                        succeeded: state == "COMPLETED",
                                        output: stdout,
                                        error: stderr,
                                        start_time,
                                        exit_code: i32::try_from(
                                            job["exit_code"].as_i64().unwrap(),
                                        )
                                        .unwrap_or(-1i32),
                                        killed,
                                        ..TaskAttempt::default()
                                    },
                                })
                                .unwrap();
                            break;
                        }
                        // Retry
                        "NODE_FAIL" | "PREEMPTED" | "BOOT_FAIL" | "DEADLINE" => {
                            let stderr = slurp_if_exists(
                                job["standard_error"].as_str().unwrap().to_string(),
                            );
                            let stdout = slurp_if_exists(
                                job["standard_output"].as_str().unwrap().to_string(),
                            );
                            response
                                .send(RunnerMessage::ExecutionReport {
                                    run_id,
                                    task_id,
                                    attempt: TaskAttempt {
                                        succeeded: false,
                                        output: stdout,
                                        error: stderr,
                                        start_time,
                                        executor: vec![format!(
                                            "Job failed due to potential cluster issue: {}",
                                            state
                                        )],
                                        exit_code: i32::try_from(
                                            job["exit_code"].as_i64().unwrap(),
                                        )
                                        .unwrap_or(-1i32),
                                        ..TaskAttempt::default()
                                    },
                                })
                                .unwrap();
                            return;
                        } // Waiting for progress
                        // "PENDING" | "SUSPENDED" | "RUNNING" => {}
                        _ => {}
                    }
                }
            }
        }
    }
}

pub async fn start_executor(base_url: String, mut msgs: mpsc::UnboundedReceiver<ExecutorMessage>) {
    let mut running_tasks = HashMap::<(RunID, TaskID), oneshot::Sender<JobEvent>>::new();

    let client = reqwest::Client::new();

    while let Some(msg) = msgs.recv().await {
        use ExecutorMessage::{ExecuteTask, ExpandTaskDetails, Stop, StopTask, ValidateTask};
        match msg {
            ValidateTask { details, response } => {
                let res = if let Err(e) = extract_details(&details) {
                    Err(anyhow!(e))
                } else {
                    Ok(())
                };
                response.send(res).unwrap_or(());
            }
            ExpandTaskDetails {
                details,
                parameters,
                response,
            } => {
                response
                    .send(expand_task_details(details, &parameters))
                    .unwrap_or(());
            }
            ExecuteTask {
                run_id,
                task_id,
                details,
                response,
                tracker,
            } => {
                let url = base_url.clone();
                match submit_slurm_job(&base_url, &client, &task_id, &details).await {
                    Ok(slurm_id) => {
                        let (kill_tx, kill_rx) = oneshot::channel();
                        let tid = task_id.clone();
                        tokio::spawn(async move {
                            watch_job(slurm_id, run_id, tid, details, url, response, kill_rx).await;
                        });
                        let (tx, _) = oneshot::channel();
                        tracker
                            .send(TrackerMessage::UpdateTaskState {
                                run_id,
                                task_id: task_id.clone(),
                                state: State::Running,
                                response: tx,
                            })
                            .unwrap_or(());
                        running_tasks.insert((run_id, task_id), kill_tx);
                    }
                    Err(e) => {
                        let mut attempt = TaskAttempt::new();
                        attempt.executor.push(format!("{:?}", e));
                        response
                            .send(RunnerMessage::ExecutionReport {
                                run_id,
                                task_id,
                                attempt,
                            })
                            .unwrap_or(());
                    }
                }
            }
            StopTask {
                run_id,
                task_id,
                response,
            } => {
                if let Some(channel) = running_tasks.remove(&(run_id, task_id)) {
                    channel.send(JobEvent::Kill).unwrap_or(());
                }
                response.send(()).unwrap_or(());
            }
            Stop {} => {
                break;
            }
        }
    }
}

pub fn start(base_url: String, msgs: mpsc::UnboundedReceiver<ExecutorMessage>) {
    tokio::spawn(async move {
        start_executor(base_url, msgs).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::process::Command;
    use users::get_current_username;

    async fn get_userinfo() -> (String, String) {
        let osuser = get_current_username().unwrap();
        let user = osuser.to_string_lossy().clone();

        let output = Command::new("scontrol")
            .arg("token")
            .output()
            .await
            .expect("Failed to execute scontrol to obtain token");

        let result = String::from_utf8_lossy(&output.stdout);
        let token = result
            .split("=")
            .nth(1)
            .expect("Unable to get token for slurm")
            .trim();

        (user.to_string(), token.to_string())
    }

    #[tokio::test]
    async fn test_basic_submission() {
        let (user, token) = get_userinfo().await;
        let base_url = "http://localhost:6820/slurm/v0.0.36".to_owned();

        let (exe_tx, exe_rx) = mpsc::unbounded_channel();
        super::start(base_url, exe_rx);

        let task_spec = format!(
            r#"
                {{
                    "command": [ "/usr/bin/echo", "hello", "$MYVAR" ],
                    "user": "{}",
                    "jwt_token": "{}",
                    "environment": {{
                        "MYVAR": "fancy_pants"
                    }},
                    "logdir": "/tmp"
                }}"#,
            user, token
        );

        let details: TaskDetails = serde_json::from_str(task_spec.as_str()).unwrap();
        let task_id = "test_task".to_owned();
        let run_id: RunID = 0;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let (log_tx, _) = mpsc::unbounded_channel();
        exe_tx
            .send(ExecutorMessage::ExecuteTask {
                run_id,
                task_id,
                details,
                response: tx,
                tracker: log_tx,
            })
            .unwrap();

        match rx.recv().await.unwrap() {
            RunnerMessage::ExecutionReport { attempt, .. } => {
                assert!(attempt.succeeded);
                assert_eq!(attempt.output, "hello fancy_pants\n");
            }
            _ => {
                assert!("Unexpected Message" == "");
            }
        }

        // Read the output

        exe_tx.send(ExecutorMessage::Stop {}).unwrap();
    }

    #[tokio::test]
    async fn test_stop_job() {
        let (user, token) = get_userinfo().await;
        let base_url = "http://localhost:6820/slurm/v0.0.36".to_owned();

        let (exe_tx, exe_rx) = mpsc::unbounded_channel();
        super::start(base_url, exe_rx);

        let task_spec = format!(
            r#"
                {{
                    "command": [ "sleep", "1800" ],
                    "user": "{}",
                    "jwt_token": "{}",
                    "logdir": "/tmp"
                }}"#,
            user, token
        );

        let details: TaskDetails = serde_json::from_str(task_spec.as_str()).unwrap();
        let task_id = "test_task".to_owned();
        let run_id: RunID = 0;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let (log_tx, _) = mpsc::unbounded_channel();
        exe_tx
            .send(ExecutorMessage::ExecuteTask {
                run_id,
                task_id: task_id.clone(),
                details,
                response: tx,
                tracker: log_tx,
            })
            .unwrap();

        // Sleep for a bit
        sleep(Duration::from_secs(2)).await;

        // Cancel
        let (cancel_tx, cancel_rx) = oneshot::channel();
        exe_tx
            .send(ExecutorMessage::StopTask {
                run_id,
                task_id,
                response: cancel_tx,
            })
            .unwrap();

        cancel_rx.await.unwrap();

        match rx.recv().await.unwrap() {
            RunnerMessage::ExecutionReport { attempt, .. } => {
                assert!(attempt.killed);
            }
            _ => {
                panic!("Unexpected Message");
            }
        }

        exe_tx.send(ExecutorMessage::Stop {}).unwrap();
    }
}
