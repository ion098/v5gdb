//! Main debugger loop and event handling logic.

use core::{convert::Infallible, mem};

use derive_more::From;
use gdbstub::{
    conn::Connection,
    stub::{
        GdbStub, GdbStubBuilder, GdbStubError, MultiThreadStopReason, SingleThreadStopReason,
        state_machine::GdbStubStateMachine,
    },
};
use snafu::Snafu;
use spin::{Mutex, MutexGuard, Once};
use static_cell::StaticCell;
use zynq7000::devcfg;

use crate::{
    Debugger,
    cpu::debug::DebugEventReason,
    debugger::sdk::InternalBreakpoint,
    exceptions::DebugEventContext,
    gdb_target::{V5Target, breakpoint::hardware::Specificity},
    sys::{DebuggerSystem, System},
    transport::{Transport, TransportError},
};

pub mod sdk;

#[derive(Debug, Snafu)]
pub enum DebuggerError {
    #[snafu(context(false))]
    Io { source: TransportError },
    GdbStub {
        inner: GdbStubError<Infallible, TransportError>,
    },
}

impl From<GdbStubError<Infallible, TransportError>> for DebuggerError {
    fn from(value: GdbStubError<Infallible, TransportError>) -> Self {
        Self::GdbStub { inner: value }
    }
}

/// Debugger manager.
pub struct V5Debugger<S: Transport> {
    session: Mutex<DebugSession<'static, S>>,
}

impl<S: Transport> V5Debugger<S> {
    /// Creates a new debugger.
    ///
    /// This function can only be called once per program run because the debugger will attempt to
    /// claim a global packet buffer.
    #[must_use]
    pub fn new(stream: S) -> Self {
        const PACKET_BUFFER_SIZE: usize = 4096;
        // Stored as a global to help limit stack usage.
        static PACKET_BUFFER: StaticCell<[u8; PACKET_BUFFER_SIZE]> = StaticCell::new();

        let pkt_buffer = PACKET_BUFFER
            .try_init_with(|| [0; _])
            .expect("Tried to claim packet buffer twice");

        let target = V5Target::new(&mut unsafe { devcfg::Registers::new_mmio_fixed() });

        Self {
            session: Mutex::new(DebugSession {
                stage: SessionStage::Uninitialized(
                    GdbStubBuilder::new(stream)
                        .with_packet_buffer(pkt_buffer)
                        .build()
                        .unwrap(),
                ),
                target,
                internal_breaks: None,
            }),
        }
    }

    /// Returns the debugger's internal state.
    #[must_use]
    pub fn session<'a>(&'a self) -> MutexGuard<'a, DebugSession<'static, S>> {
        self.session.lock()
    }
}

unsafe impl<S: Transport + 'static> Debugger for V5Debugger<S> {
    fn initialize(&self) {
        let mut session = self.session();
        session.register_internal_breakpoints();
        System::initialize(&mut session.target);
        crate::sdk::competition::install_override();
        log::debug!("Debugger initialized");
    }

    unsafe fn handle_debug_event(&self, ctx: &mut DebugEventContext) -> bool {
        let mut session = self.session();
        // Pause software breakpoints before allowing unpredictable control flow (by interrupts).
        session.target.set_breakpoints_ignored(true);

        // We re-enable interrupts after the abort (so that UART works) but prevent the RTOS from
        // preempting us. When the debugger is active, the system should appear paused.

        // If we're handling a single-step completion, the scheduler is already disabled from when
        // the step was initiated (previous debug session), so there's no need to do that again.
        if session.target.single_step_request.is_none() {
            System::suspend_preemption();
        }
        unsafe {
            aarch32_cpu::interrupt::enable();
        }

        log::debug!("Entered debug event handler");
        static BKPT_LOG: Once = Once::new();
        BKPT_LOG.call_once(|| {
            log::error!("**** v5gdb: BREAKPOINT TRIGGERED ****");
            log::error!("Your program has been paused. Please connect a debugger.")
        });

        let was_locked = session.target.hw_manager.locked();
        session.target.hw_manager.set_locked(false);
        session.target.exception_ctx = ctx.clone();

        let reason = session.target.hw_manager.last_break_reason();

        let bkpt_address = session.target.exception_ctx.program_counter;
        let tracked_bkpt_id = session.target.query_sw_breakpoint(bkpt_address);

        session.target.last_stop_was_hardcoded =
            tracked_bkpt_id.is_none() && reason == Some(DebugEventReason::BkptInstr);

        // If we previously wanted to single step, we can permanently remove the breakpoint that
        // supported that now. The single step request is then cleared since we've finished all
        // required cleanup.
        if let Some(single_step) = session.target.single_step_request.take() {
            session.target.hw_manager.remove_breakpoint_at(
                single_step.target_addr,
                Specificity::Mismatch,
                single_step.kind,
            );
        }

        if session.target.last_stop_was_hardcoded {
            // Normally we try to avoid an infinite loop of breakpoints by replacing tracked
            // software breakpoints with their real instructions and re-running them. But if the
            // `bkpt` *is* the real instruction then we don't need to do the normal
            // replace-and-rerun thing. Instead, we just skip over it because its side-effect has
            // been completed.

            // SAFETY: Since the address was able to be properly fetched, it implies it is valid for
            // reads.
            let instr = unsafe { session.target.exception_ctx.read_instr() };
            session.target.exception_ctx.program_counter += instr.size() as u32;
        }

        let mut show_debug_console = true;

        if let Some(id) = tracked_bkpt_id
            && let Some(bkpt) = session.target.breaks[id]
        {
            // Some tracked breakpoints weren't requested by the user and are just used internally.
            // These should be transparent to the user by default. Note: It's possible
            // for a breakpoint to be both requested by the user and used internally.
            show_debug_console = bkpt.reason.user;

            // If this breakpoint is used internally, run any necessary callbacks.
            if bkpt.reason.internal {
                show_debug_console |= session.handle_internal_breakpoint();
            }
        }

        if show_debug_console {
            log::debug!("Starting debug console");
            session.run_debug_console();
            log::debug!("Debug console has exited");
        }

        // Write any modifications back to the stack so the assembly code restores the updated state
        *ctx = session.target.exception_ctx.clone();

        log::debug!("Exiting debug event handler");

        // Single steps run with the scheduler off so that we are guaranteed to step the current
        // task, not a different one. - Side note: If PROS implemented ARM's context id register, we
        // could just filter the single step breakpoint by task id and there would be no need for
        // this.
        let should_unpause_scheduler = session.target.single_step_request.is_none();

        session.target.hw_manager.set_locked(was_locked);
        session.target.set_breakpoints_ignored(false);

        should_unpause_scheduler
    }
}

/// Handles the GDB protocol lifecycle.
pub struct DebugSession<'a, S>
where
    S: Transport,
{
    pub target: V5Target,
    internal_breaks: Option<[(InternalBreakpoint, u32); 1]>,
    stage: SessionStage<'a, S>,
}

#[derive(From)]
enum SessionStage<'a, C: Connection> {
    /// Remote has not yet connected / been configured.
    Uninitialized(GdbStub<'a, V5Target, C>),
    /// Session is running.
    Active(GdbStubStateMachine<'a, V5Target, C>),
    /// Placeholder while transitioning between states.
    Transitioning,
}

impl<S> DebugSession<'_, S>
where
    S: Transport,
{
    fn has_client(&self) -> bool {
        match &self.stage {
            SessionStage::Active(GdbStubStateMachine::Disconnected(_)) => false,
            SessionStage::Active(_) => true,
            _ => false,
        }
    }

    /// Runs the debug console until the user indicates they want to continue program execution.
    fn run_debug_console(&mut self) {
        let stage = mem::replace(&mut self.stage, SessionStage::Transitioning);
        match stage {
            SessionStage::Uninitialized(gdb) => {
                self.stage = gdb.run_state_machine(&mut self.target).unwrap().into();
                self.run_debug_console();
            }
            SessionStage::Active(mut state) => {
                self.target.reset_resume();
                while !self.target.resume {
                    unsafe {
                        vex_sdk::vexTasksRun();
                    }

                    state = Self::tick_state_machine(state, &mut self.target)
                        .expect("debugger encountered an error");
                }

                self.target.resume = false;
                self.stage = state.into();
            }
            SessionStage::Transitioning => panic!("Cannot resume from transitioning state"),
        }
    }

    fn tick_state_machine<'a>(
        gdb: GdbStubStateMachine<'a, V5Target, S>,
        target: &mut V5Target,
    ) -> Result<GdbStubStateMachine<'a, V5Target, S>, DebuggerError> {
        match gdb {
            GdbStubStateMachine::Idle(mut gdb) => {
                if let Ok(byte) = gdb.borrow_conn().read() {
                    Ok(gdb.incoming_data(target, byte)?)
                } else {
                    Ok(gdb.into())
                }
            }
            GdbStubStateMachine::Running(gdb) => {
                let reported_reason = target.get_stop_reason();
                log::info!("Debugger Stop reason: {reported_reason:?}");

                // Once we tell GDB we've exited we should exit the monitor because the session will
                // end.
                if matches!(reported_reason, MultiThreadStopReason::Exited(_)) {
                    target.resume = true;
                }

                Ok(gdb.report_stop(target, reported_reason)?)
            }
            GdbStubStateMachine::CtrlCInterrupt(gdb) => {
                log::warn!("Got Ctrl+C");
                let stop_reason: Option<SingleThreadStopReason<_>> = None;
                Ok(gdb.interrupt_handled(target, stop_reason)?)
            }
            GdbStubStateMachine::Disconnected(gdb) => Ok(gdb.return_to_idle()),
        }
    }
}
