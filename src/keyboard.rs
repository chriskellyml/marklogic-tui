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

pub(crate) fn enable() -> io::Result<()> {
    if supports_keyboard_enhancement().unwrap_or(false) {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        ENABLED.store(true, Ordering::Relaxed);
    }
    Ok(())
}

pub(crate) fn suspend(output: &mut impl Write) -> io::Result<()> {
    if ENABLED.load(Ordering::Relaxed) {
        execute!(output, PopKeyboardEnhancementFlags)?;
    }
    Ok(())
}

pub(crate) fn resume(output: &mut impl Write) -> io::Result<()> {
    if ENABLED.load(Ordering::Relaxed) {
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
