use crate::server::autoalloc::state::{Allocation, AllocationEvent, AllocationStatus};
use crate::server::autoalloc::DescriptorId;
use crate::server::state::StateRef;
use std::time::SystemTime;

macro_rules! get_or_return {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            _ => return,
        }
    };
}

macro_rules! get_or_continue {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            _ => continue,
        }
    };
}

/// The main entrypoint of the autoalloc background process.
/// It invokes the autoalloc logic in fixed time intervals.
pub async fn autoalloc_process(state_ref: StateRef) {
    let duration = state_ref.get().get_autoalloc_state().refresh_interval();
    let mut interval = tokio::time::interval(duration);
    loop {
        interval.tick().await;
        autoalloc_tick(&state_ref).await;
    }
}

/// Removes all remaining active allocations
pub async fn autoalloc_shutdown(state_ref: StateRef) {
    let futures: Vec<_> = {
        state_ref
            .get()
            .get_autoalloc_state()
            .descriptors()
            .flat_map(|(_, descriptor)| {
                let handler = descriptor.descriptor.handler();
                descriptor
                    .all_allocations()
                    .filter(|alloc| alloc.is_active())
                    .map(move |alloc| {
                        let fut = handler.remove_allocation(alloc.id.clone());
                        let id = alloc.id.clone();
                        async move { (fut.await, id) }
                    })
            })
            .collect()
    };

    for (result, allocation_id) in futures::future::join_all(futures).await {
        match result {
            Ok(_) => {
                log::info!("Allocation {} was removed", allocation_id);
            }
            Err(e) => {
                log::error!("Failed to remove allocation {}: {:?}", allocation_id, e);
            }
        }
    }
}

async fn autoalloc_tick(state_ref: &StateRef) {
    log::debug!("Running autoalloc");

    // The descriptor names are copied out to avoid holding state reference across `await`
    let descriptor_ids: Vec<DescriptorId> = state_ref
        .get()
        .get_autoalloc_state()
        .descriptor_ids()
        .collect();

    for id in descriptor_ids {
        process_descriptor(id, state_ref).await;
    }
}

async fn process_descriptor(id: DescriptorId, state: &StateRef) {
    // TODO: check only once in a while
    refresh_allocations(id, state).await;
    schedule_new_allocations(id, state).await
}

/// Go through the allocations of descriptor with the given name and refresh their status.
/// Queue allocations might become running or finished, running allocations might become finished,
/// etc.
#[allow(clippy::needless_collect)]
async fn refresh_allocations(id: DescriptorId, state_ref: &StateRef) {
    let allocation_ids: Vec<_> =
        get_or_return!(state_ref.get().get_autoalloc_state().get_descriptor(id))
            .active_allocations()
            .map(|alloc| alloc.id.clone())
            .collect();
    for allocation_id in allocation_ids.into_iter() {
        let status_fut = get_or_return!(state_ref.get().get_autoalloc_state().get_descriptor(id))
            .descriptor
            .handler()
            .get_allocation_status(allocation_id.clone());

        let result = status_fut.await;

        let mut state = state_ref.get_mut();
        let state = state.get_autoalloc_state_mut();
        match result {
            Ok(status) => {
                match status {
                    Some(status) => {
                        let descriptor = get_or_continue!(state.get_descriptor_mut(id));
                        let id = allocation_id.clone();
                        log::debug!("Status of allocation {}: {:?}", allocation_id, status);
                        match status {
                            AllocationStatus::Running { .. } => {
                                let allocation =
                                    get_or_continue!(descriptor.get_allocation_mut(&allocation_id));
                                if let AllocationStatus::Queued = allocation.status {
                                    descriptor.add_event(AllocationEvent::AllocationStarted(
                                        allocation_id,
                                    ));
                                }
                            }
                            AllocationStatus::Finished { .. } => {
                                descriptor
                                    .add_event(AllocationEvent::AllocationFinished(allocation_id));
                            }
                            AllocationStatus::Failed { .. } => {
                                descriptor
                                    .add_event(AllocationEvent::AllocationFailed(allocation_id));
                            }
                            AllocationStatus::Queued => {}
                        };
                        get_or_continue!(descriptor.get_allocation_mut(&id)).status = status;
                    }
                    None => {
                        log::warn!("Allocation {} was not found", allocation_id);
                        let descriptor = get_or_continue!(state.get_descriptor_mut(id));
                        descriptor.remove_allocation(&allocation_id);
                        descriptor.add_event(AllocationEvent::AllocationDisappeared(allocation_id));
                    }
                };
            }
            Err(err) => {
                log::error!(
                    "Failed to get allocation {} status from {}: {:?}",
                    allocation_id,
                    id,
                    err
                );
                let descriptor = get_or_continue!(state.get_descriptor_mut(id));
                descriptor.add_event(AllocationEvent::StatusFail {
                    error: err.to_string(),
                });
            }
        }
    }
}

/// Schedule new allocations for the descriptor with the given name.
async fn schedule_new_allocations(id: DescriptorId, state_ref: &StateRef) {
    let (mut remaining, max_workers_per_alloc): (u64, u64) = {
        let state = state_ref.get();
        let descriptor = get_or_return!(state.get_autoalloc_state().get_descriptor(id));
        let active_workers: u64 = descriptor
            .active_allocations()
            .map(|alloc| alloc.worker_count)
            .sum();

        let descriptor_impl = &descriptor.descriptor;
        let scale = descriptor_impl.info().target_worker_count();
        (
            scale.saturating_sub(active_workers as u32) as u64,
            descriptor_impl.info().max_workers_per_alloc() as u64,
        )
    };
    while remaining > 0 {
        let to_schedule = std::cmp::min(remaining, max_workers_per_alloc);
        let schedule_fut = get_or_return!(state_ref.get().get_autoalloc_state().get_descriptor(id))
            .descriptor
            .handler()
            .schedule_allocation(to_schedule);

        let result = schedule_fut.await;

        let mut state = state_ref.get_mut();
        let state = state.get_autoalloc_state_mut();
        let descriptor = get_or_return!(state.get_descriptor_mut(id));
        match result {
            Ok(created) => {
                log::info!("Queued {} workers into {}", to_schedule, id);
                descriptor.add_event(AllocationEvent::AllocationQueued(created.id().to_string()));
                descriptor.add_allocation(Allocation {
                    id: created.id().to_string(),
                    worker_count: to_schedule,
                    queued_at: SystemTime::now(),
                    status: AllocationStatus::Queued,
                    working_dir: created.working_dir().to_path_buf(),
                });
            }
            Err(err) => {
                log::error!("Failed to queue allocation into {}: {:?}", id, err);
                descriptor.add_event(AllocationEvent::QueueFail {
                    error: err.to_string(),
                });
            }
        }

        remaining -= to_schedule;
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::time::{Duration, SystemTime};

    use crate::common::manager::info::ManagerType;
    use crate::common::WrappedRcRefCell;
    use crate::server::autoalloc::descriptor::{
        CreatedAllocation, QueueDescriptor, QueueHandler, QueueInfo,
    };
    use crate::server::autoalloc::process::autoalloc_tick;
    use crate::server::autoalloc::state::{AllocationEvent, AllocationId, AllocationStatus};
    use crate::server::autoalloc::AutoAllocResult;
    use crate::server::state::StateRef;
    use std::pin::Pin;

    #[tokio::test]
    async fn test_do_not_overallocate_queue() {
        let state = create_state();
        let call_count = WrappedRcRefCell::wrap(0);

        let handler = Handler::new(
            call_count.clone(),
            move |s, _| async move {
                *s.get_mut() += 1;
                Ok(CreatedAllocation::new("1".to_string(), "".into()))
            },
            move |_, _| async move { Ok(Some(AllocationStatus::Queued)) },
            |_, _| async move { Ok(()) },
        );
        add_descriptor(&state, handler, 1, 1);

        autoalloc_tick(&state).await;
        autoalloc_tick(&state).await;

        assert_eq!(*call_count.get(), 1);
    }

    #[tokio::test]
    async fn test_split_allocations_per_max_job_size() {
        let state = create_state();

        struct State {
            requests: Vec<u64>,
        }

        let call_count = WrappedRcRefCell::wrap(State {
            requests: vec![3, 3, 3, 1],
        });

        let handler = Handler::new(
            call_count.clone(),
            move |s, worker_count| async move {
                let mut state = s.get_mut();
                assert_eq!(worker_count, state.requests[0]);
                state.requests.remove(0);
                Ok(CreatedAllocation::new(
                    state.requests.len().to_string(),
                    "".into(),
                ))
            },
            move |_, _| async move { Ok(Some(AllocationStatus::Queued)) },
            |_, _| async move { Ok(()) },
        );
        add_descriptor(&state, handler, 10, 3);

        autoalloc_tick(&state).await;

        assert_eq!(call_count.get().requests.len(), 0);
    }

    #[tokio::test]
    async fn test_log_failed_allocation_attempt() {
        let state = create_state();

        let handler = Handler::new(
            WrappedRcRefCell::wrap(()),
            move |_, _| async move { anyhow::bail!("foo") },
            move |_, _| async move { Ok(Some(AllocationStatus::Queued)) },
            |_, _| async move { Ok(()) },
        );
        add_descriptor(&state, handler, 1, 1);

        autoalloc_tick(&state).await;

        let state = state.get();
        let state = state.get_autoalloc_state();
        let descriptor = state.get_descriptor(0).unwrap();
        matches!(
            descriptor.get_events()[0].event,
            AllocationEvent::QueueFail { .. }
        );
    }

    #[tokio::test]
    async fn test_reschedule_after_job_ends() {
        let state = create_state();

        struct State {
            job_id: u64,
            status: Option<AllocationStatus>,
        }

        let custom_state = WrappedRcRefCell::wrap(State {
            job_id: 0,
            status: None,
        });

        let handler = Handler::new(
            custom_state.clone(),
            move |s, _| async move {
                s.get_mut().job_id += 1;
                Ok(CreatedAllocation::new(
                    s.get().job_id.to_string(),
                    "".into(),
                ))
            },
            move |s, _| async move { Ok(s.get().status.clone()) },
            |_, _| async move { Ok(()) },
        );
        add_descriptor(&state, handler, 1, 1);

        autoalloc_tick(&state).await;
        custom_state.get_mut().status = Some(AllocationStatus::Queued);
        autoalloc_tick(&state).await;
        custom_state.get_mut().status = Some(AllocationStatus::Finished {
            started_at: SystemTime::now(),
            finished_at: SystemTime::now(),
        });
        autoalloc_tick(&state).await;

        assert_eq!(custom_state.get().job_id, 2);
    }

    struct Handler<ScheduleFn, StatusFn, RemoveFn, State> {
        schedule_fn: WrappedRcRefCell<ScheduleFn>,
        status_fn: WrappedRcRefCell<StatusFn>,
        remove_fn: WrappedRcRefCell<RemoveFn>,
        custom_state: WrappedRcRefCell<State>,
    }

    impl<
            State: 'static,
            ScheduleFn: 'static + Fn(WrappedRcRefCell<State>, u64) -> ScheduleFnFut,
            ScheduleFnFut: Future<Output = AutoAllocResult<CreatedAllocation>>,
            StatusFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> StatusFnFut,
            StatusFnFut: Future<Output = AutoAllocResult<Option<AllocationStatus>>>,
            RemoveFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> RemoveFnFut,
            RemoveFnFut: Future<Output = AutoAllocResult<()>>,
        > Handler<ScheduleFn, StatusFn, RemoveFn, State>
    {
        fn new(
            custom_state: WrappedRcRefCell<State>,
            schedule_fn: ScheduleFn,
            status_fn: StatusFn,
            remove_fn: RemoveFn,
        ) -> Box<dyn QueueHandler> {
            Box::new(Self {
                schedule_fn: WrappedRcRefCell::wrap(schedule_fn),
                status_fn: WrappedRcRefCell::wrap(status_fn),
                remove_fn: WrappedRcRefCell::wrap(remove_fn),
                custom_state,
            })
        }
    }

    impl<
            State: 'static,
            ScheduleFn: 'static + Fn(WrappedRcRefCell<State>, u64) -> ScheduleFnFut,
            ScheduleFnFut: Future<Output = AutoAllocResult<CreatedAllocation>>,
            StatusFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> StatusFnFut,
            StatusFnFut: Future<Output = AutoAllocResult<Option<AllocationStatus>>>,
            RemoveFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> RemoveFnFut,
            RemoveFnFut: Future<Output = AutoAllocResult<()>>,
        > QueueHandler for Handler<ScheduleFn, StatusFn, RemoveFn, State>
    {
        fn schedule_allocation(
            &self,
            worker_count: u64,
        ) -> Pin<Box<dyn Future<Output = AutoAllocResult<CreatedAllocation>>>> {
            let schedule_fn = self.schedule_fn.clone();
            let custom_state = self.custom_state.clone();

            Box::pin(async move { (schedule_fn.get())(custom_state.clone(), worker_count).await })
        }

        fn get_allocation_status(
            &self,
            allocation_id: AllocationId,
        ) -> Pin<Box<dyn Future<Output = AutoAllocResult<Option<AllocationStatus>>>>> {
            let status_fn = self.status_fn.clone();
            let custom_state = self.custom_state.clone();

            Box::pin(async move { (status_fn.get())(custom_state.clone(), allocation_id).await })
        }

        fn remove_allocation(
            &self,
            allocation_id: AllocationId,
        ) -> Pin<Box<dyn Future<Output = AutoAllocResult<()>>>> {
            let remove_fn = self.remove_fn.clone();
            let custom_state = self.custom_state.clone();

            Box::pin(async move { (remove_fn.get())(custom_state.clone(), allocation_id).await })
        }
    }

    fn add_descriptor(
        state_ref: &StateRef,
        handler: Box<dyn QueueHandler>,
        target_worker_count: u32,
        max_workers_per_alloc: u32,
    ) {
        let descriptor = QueueDescriptor::new(
            ManagerType::Pbs,
            QueueInfo::new(
                "queue".to_string(),
                max_workers_per_alloc,
                target_worker_count,
                None,
            ),
            None,
            handler,
        );

        state_ref
            .get_mut()
            .get_autoalloc_state_mut()
            .add_descriptor(0, descriptor)
    }

    fn create_state() -> StateRef {
        StateRef::new(Duration::from_millis(100))
    }
}
