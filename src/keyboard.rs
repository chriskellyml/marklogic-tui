//! Negotiate unambiguous modified keys where the terminal supports them.
use crossterm::{
    event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags},
    execute,
    terminal::supports_keyboard_enhancement,
};
use std::{
    io::{self, Write},
    sync::atomic::{AtomicBool, Ordering},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn enable() -> io::Result<()> {
    if supports_keyboard_enhancement().unwrap_or(false) {
        // Record the intended state first so cleanup can undo a partial write.
        ENABLED.store(true, Ordering::Relaxed);
        ACTIVE.store(true, Ordering::Relaxed);
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    Ok(())
}

pub(crate) fn suspend(output: &mut impl Write) -> io::Result<()> {
    if ACTIVE.load(Ordering::Relaxed) {
        execute!(output, PopKeyboardEnhancementFlags)?;
        ACTIVE.store(false, Ordering::Relaxed);
    }
    Ok(())
}

pub(crate) fn resume(output: &mut impl Write) -> io::Result<()> {
    if ENABLED.load(Ordering::Relaxed) && !ACTIVE.load(Ordering::Relaxed) {
        ACTIVE.store(true, Ordering::Relaxed);
        execute!(
            output,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    Ok(())
}

pub(crate) fn disable(output: &mut impl Write) -> io::Result<()> {
    suspend(output)?;
    ENABLED.store(false, Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspension_resume_and_shutdown_balance_keyboard_modes() {
        ENABLED.store(true, Ordering::Relaxed);
        ACTIVE.store(true, Ordering::Relaxed);
        let mut output = Vec::new();
        suspend(&mut output).unwrap();
        suspend(&mut output).unwrap();
        resume(&mut output).unwrap();
        resume(&mut output).unwrap();
        disable(&mut output).unwrap();
        disable(&mut output).unwrap();
        assert_eq!(output, b"\x1b[<1u\x1b[>1u\x1b[<1u");
        assert!(!ENABLED.load(Ordering::Relaxed));
        assert!(!ACTIVE.load(Ordering::Relaxed));
    }
}
