use std::io;
use std::process::ExitStatus;
use std::time::Duration;

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

/// A child process that owns its entire descendant tree.
///
/// On Unix this is a fresh process group; on Windows it is a job object.  The
/// explicit `Drop` implementation closes the last lifecycle hole left by
/// cancellation: it terminates the group/job even when an async wait is
/// abandoned before a caller can reap it.
#[derive(Debug)]
pub(crate) struct ManagedChild {
    child: Box<dyn ChildWrapper>,
    finished: bool,
}

impl ManagedChild {
    pub(crate) fn spawn(command: Command) -> io::Result<Self> {
        let mut command = CommandWrap::from(command);
        #[cfg(unix)]
        command.wrap(ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(JobObject);
        command.wrap(KillOnDrop);
        command.spawn().map(|child| Self {
            child,
            finished: false,
        })
    }
    pub(crate) fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin().take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout().take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.stderr().take()
    }

    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            self.finished = true;
        }
        status
    }

    pub(crate) async fn wait_bounded(
        &mut self,
        duration: Duration,
    ) -> io::Result<Option<ExitStatus>> {
        match timeout(duration, self.wait()).await {
            Ok(status) => status.map(Some),
            Err(_) => Ok(None),
        }
    }

    /// Gracefully end the tree where the operating system provides signals,
    /// then forcibly terminate and reap it if the bounded grace wait expires.
    pub(crate) async fn terminate_tree(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        self.child.signal(libc::SIGTERM)?;

        if let Some(status) = self.wait_bounded(grace).await? {
            return Ok(status);
        }

        self.force_terminate_tree(grace).await
    }

    /// Terminate every process in the owned group/job and wait a bounded
    /// interval for the leader to be reaped.
    pub(crate) async fn force_terminate_tree(
        &mut self,
        reap_grace: Duration,
    ) -> io::Result<ExitStatus> {
        self.child.start_kill()?;
        self.wait_bounded(reap_grace).await?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out reaping owned process tree after force termination",
            )
        })
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if !self.finished {
            // `KillOnDrop` protects the immediate child. Calling through the
            // outer wrapper makes cancellation kill the whole process group/job.
            let _ = self.child.start_kill();
        }
    }
}
