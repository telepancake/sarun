//! Sarun adapter for Chupa's mirror supervisor.

pub use chupa::supervisor::{
    Job, job_add, job_add_with_media, job_cancel, job_register_paused, job_remove,
    job_remove_with_data, job_run, job_run_backrefs, job_run_full, job_run_images,
    job_set_media_source, job_set_paused, jobs_list, run_pending, scheduler_thread, stop_all,
};

pub fn configure_state_home(path: impl Into<std::path::PathBuf>) {
    chupa::supervisor::set_state_home(path);
}

pub fn jobs_list_typed() -> Result<Vec<crate::generated_wire::MirrorJob>, String> {
    use crate::generated_wire::{MirrorJob, MirrorState};
    jobs_list()?
        .into_iter()
        .map(|job| {
            let state = match job.state.as_str() {
                "starting" | "running" | "stopping" => MirrorState::Running,
                "deleting" | "cancelled" | "interrupted" => MirrorState::Stopped,
                "pending" => MirrorState::Pending,
                "error" => MirrorState::Error,
                "completed" => MirrorState::Completed,
                state => return Err(format!("unknown derived mirror state {state:?}")),
            };
            Ok(MirrorJob {
                id: u64::try_from(job.id).map_err(|_| "negative mirror job id")?,
                kind: crate::wire::BoundedText::new(job.kind)
                    .map_err(|error| format!("mirror kind exceeds relation bound: {error:?}"))?,
                source: crate::wire::BoundedText::new(job.src)
                    .map_err(|error| format!("mirror source exceeds relation bound: {error:?}"))?,
                destination: crate::wire::BoundedBytes::new(job.dest.into_bytes())
                    .map_err(|error| format!("mirror destination exceeds relation bound: {error:?}"))?,
                interval_seconds: u64::try_from(job.interval_secs)
                    .map_err(|_| "negative mirror interval")?,
                paused: job.paused,
                last_start: job.last_start,
                last_end: job.last_end,
                last_exit: job
                    .last_exit
                    .map(|exit| i32::try_from(exit).map_err(|_| "mirror exit code exceeds i32"))
                    .transpose()?,
                last_detail: crate::wire::BoundedText::new(job.last_detail)
                    .map_err(|error| format!("mirror detail exceeds relation bound: {error:?}"))?,
                state,
                next_due: job.next_due,
            })
        })
        .collect()
}
