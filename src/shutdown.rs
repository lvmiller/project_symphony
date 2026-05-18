use std::future::{Future, pending};

/// Waits for an operator or container shutdown request.
pub async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            eprintln!("signal_error error=\"failed to install SIGINT handler: {error}\"");
        }
    };
    wait_for_shutdown_event(interrupt, terminate_signal()).await;
}

async fn wait_for_shutdown_event<I, T>(interrupt: I, terminate: T)
where
    I: Future<Output = ()>,
    T: Future<Output = ()>,
{
    tokio::pin!(interrupt);
    tokio::pin!(terminate);
    tokio::select! {
        _ = &mut interrupt => {}
        _ = &mut terminate => {}
    }
}

#[cfg(unix)]
async fn terminate_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut signal = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(error) => {
            eprintln!("signal_error error=\"failed to install SIGTERM handler: {error}\"");
            pending::<()>().await;
            return;
        }
    };
    signal.recv().await;
}

#[cfg(not(unix))]
async fn terminate_signal() {
    pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn shutdown_event_returns_when_interrupt_branch_completes() {
        timeout(
            Duration::from_millis(100),
            wait_for_shutdown_event(async {}, pending::<()>()),
        )
        .await
        .expect("interrupt branch should complete shutdown wait");
    }

    #[tokio::test]
    async fn shutdown_event_returns_when_terminate_branch_completes() {
        timeout(
            Duration::from_millis(100),
            wait_for_shutdown_event(pending::<()>(), async {}),
        )
        .await
        .expect("terminate branch should complete shutdown wait");
    }
}
