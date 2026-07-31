//! Application-owned Kitty graphics operation queue.
//!
//! Generated-buffer code may enqueue bounded PNG uploads and deletions, but
//! only the terminal backend drains them as part of synchronized drawing.

use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
};

use base64::{engine::general_purpose::STANDARD, Engine as _};

const MAX_PNG_BYTES: usize = 12 * 1024 * 1024;
const CHUNK: usize = 4096;
const MAX_PENDING_OPERATIONS: usize = 256;
const MAX_PENDING_PNG_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Operation {
    Upload {
        id: u32,
        png: Vec<u8>,
        columns: u16,
        rows: u16,
    },
    Delete(u32),
    DeleteAll,
}

static AVAILABLE: AtomicBool = AtomicBool::new(false);
static OPERATIONS: OnceLock<Mutex<VecDeque<Operation>>> = OnceLock::new();

fn operations() -> &'static Mutex<VecDeque<Operation>> {
    OPERATIONS.get_or_init(|| Mutex::new(VecDeque::new()))
}

pub fn set_available(available: bool) {
    AVAILABLE.store(available, Ordering::Release);
    if !available {
        operations().lock().unwrap().clear();
    }
}

pub fn available() -> bool {
    AVAILABLE.load(Ordering::Acquire)
}

pub fn queue_upload(id: u32, png: Vec<u8>, columns: u16, rows: u16) -> bool {
    if !available()
        || id == 0
        || id > 0x00ff_ffff
        || png.is_empty()
        || png.len() > MAX_PNG_BYTES
        || columns == 0
        || rows == 0
    {
        return false;
    }
    let mut operations = operations().lock().unwrap();
    let pending_png_bytes = operations
        .iter()
        .map(|operation| match operation {
            Operation::Upload { png, .. } => png.len(),
            _ => 0,
        })
        .sum::<usize>();
    if operations.len() >= MAX_PENDING_OPERATIONS
        || pending_png_bytes.saturating_add(png.len()) > MAX_PENDING_PNG_BYTES
    {
        return false;
    }
    operations.push_back(Operation::Upload {
        id,
        png,
        columns,
        rows,
    });
    true
}

pub fn queue_delete(id: u32) {
    if available() && id != 0 {
        let mut operations = operations().lock().unwrap();
        if operations.len() < MAX_PENDING_OPERATIONS {
            operations.push_back(Operation::Delete(id));
        }
    }
}

pub fn queue_delete_all() {
    if available() {
        let mut operations = operations().lock().unwrap();
        operations.clear();
        operations.push_back(Operation::DeleteAll);
    }
}

pub(super) fn drain(writer: &mut impl Write) -> io::Result<()> {
    if !available() {
        return Ok(());
    }
    let pending = operations().lock().unwrap().drain(..).collect::<Vec<_>>();
    for operation in pending {
        match operation {
            Operation::Upload {
                id,
                png,
                columns,
                rows,
            } => write_upload(writer, id, &png, columns, rows)?,
            Operation::Delete(id) => {
                write!(writer, "\u{1b}_Ga=d,d=I,i={id},q=2\u{1b}\\")?;
            }
            Operation::DeleteAll => {
                write!(writer, "\u{1b}_Ga=d,d=A,q=2\u{1b}\\")?;
            }
        }
    }
    Ok(())
}

fn write_upload(
    writer: &mut impl Write,
    id: u32,
    png: &[u8],
    columns: u16,
    rows: u16,
) -> io::Result<()> {
    let encoded = STANDARD.encode(png);
    let mut chunks = encoded.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        if first {
            write!(
                writer,
                "\u{1b}_Ga=t,f=100,i={id},q=2,m={};",
                u8::from(chunks.peek().is_some())
            )?;
            first = false;
        } else {
            write!(
                writer,
                "\u{1b}_Gq=2,m={};",
                u8::from(chunks.peek().is_some())
            )?;
        }
        writer.write_all(chunk)?;
        write!(writer, "\u{1b}\\")?;
    }
    write!(
        writer,
        "\u{1b}_Ga=p,U=1,i={id},c={columns},r={rows},q=2\u{1b}\\"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn unsupported_backends_emit_nothing() {
        let _guard = TEST_LOCK.lock().unwrap();
        set_available(false);
        assert!(!queue_upload(7, vec![1, 2, 3], 2, 1));
        let mut output = Vec::new();
        drain(&mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn uploads_place_and_delete_with_quiet_protocol_operations() {
        let _guard = TEST_LOCK.lock().unwrap();
        set_available(true);
        assert!(queue_upload(42, vec![1, 2, 3], 3, 2));
        queue_delete(42);
        let mut output = Vec::new();
        drain(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output,
            "\u{1b}_Ga=t,f=100,i=42,q=2,m=0;AQID\u{1b}\\\
             \u{1b}_Ga=p,U=1,i=42,c=3,r=2,q=2\u{1b}\\\
             \u{1b}_Ga=d,d=I,i=42,q=2\u{1b}\\"
        );

        assert!(queue_upload(43, vec![1; 5000], 1, 1));
        let mut chunked = Vec::new();
        drain(&mut chunked).unwrap();
        let chunked = String::from_utf8(chunked).unwrap();
        assert!(chunked.contains("a=t,f=100,i=43,q=2,m=1"));
        assert!(chunked.contains("\u{1b}_Gq=2,m=0;"));
    }
}
