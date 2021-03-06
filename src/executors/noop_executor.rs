use super::{ExecutorMessage, RunnerMessage, TrackerMessage};
use crate::structs::{State, TaskAttempt};
use tokio::sync::{mpsc, oneshot};

pub async fn start_local_executor(mut exe_msgs: mpsc::UnboundedReceiver<ExecutorMessage>) {
    while let Some(msg) = exe_msgs.recv().await {
        use ExecutorMessage::{ExecuteTask, ExpandTaskDetails, Stop, StopTask, ValidateTask};

        match msg {
            ValidateTask { response, .. } => response.send(Ok(())).unwrap_or(()),
            ExpandTaskDetails {
                details, response, ..
            } => response.send(Ok(vec![(details, Vec::new())])).unwrap_or(()),
            ExecuteTask {
                run_id,
                task_id,
                response,
                tracker,
                ..
            } => {
                let (upd, _) = oneshot::channel();
                tracker
                    .send(TrackerMessage::UpdateTaskState {
                        run_id,
                        task_id: task_id.clone(),
                        state: State::Running,
                        response: upd,
                    })
                    .unwrap_or(());
                let mut attempt = TaskAttempt::new();
                attempt.succeeded = true;
                response
                    .send(RunnerMessage::ExecutionReport {
                        run_id,
                        task_id: task_id.clone(),
                        attempt,
                    })
                    .unwrap_or(());
            }
            StopTask { .. } => {}
            Stop {} => {
                break;
            }
        }
    }
}
