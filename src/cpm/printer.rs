//! The CP/M line printer: capture `LST:` output and leave a document behind.
//!
//! A CP/M machine's printer is a *stream of bytes with no end*. There is no
//! "open" and no "close": the program writes characters, the paper moves, and
//! whoever is standing there decides the job is finished. Everything awkward
//! about this module comes from that, and the two decisions worth reading
//! before the code are [`SpoolJob::idle_expired`] (when a job is over) and
//! [`Page`] (how a column of bytes becomes a page).
//!
//! # Two guests, two paths, one spool
//!
//! The bytes arrive from one of two completely separate places, and the split
//! is the same one that separates mounting from booting:
//!
//! * **The emulator.** Our own BDOS and BIOS are underneath the guest, so the
//!   printer is a *service*: BDOS function 5 (List Output) and the BIOS `LIST`
//!   vector. Nothing about the guest's hardware is involved and every program
//!   that prints through the operating system — WordStar, MBASIC's `LPRINT`,
//!   `PIP LST:=FILE.TXT` — arrives here.
//! * **A booted disk.** The guest owns the machine and drives a printer
//!   *board*, so we have to be one. Measured, not reasoned: Altair Hard Disk
//!   BASIC answering `LINEPRINTER? C` initialises with `OUT 03h←11h` /
//!   `OUT 02h←00h` and then sends one 7-bit ASCII byte per character to port
//!   `03h`, ending each line with a bare CR. See [`PORT_CHOICES`].
//!
//! Both feed the same [`SpoolJob`], so the document logic exists once.
//!
//! # What it does not do yet
//!
//! **No bold or underline.** Period software produces them by *overstrike* —
//! WordStar prints a line, returns with a bare CR and prints it again for
//! double-strike, or overprints a letter with `_` for underline — and this
//! module resolves overstrike into readable *text* (see [`Page::put`]) without
//! turning it into styling. That is a deliberate first step: the text is
//! correct and complete, and a later pass can recognise the overstrike
//! patterns, or an Epson `ESC E` / `ESC -1` driver, and emit real ODF spans.
//! Getting the text right first means that pass cannot silently lose content.

use std::path::Path;
use std::time::{Duration, Instant};

/// Value of `cpm_printer` that captures nothing.
pub const PRINTER_OFF: &str = "off";
/// Value of `cpm_printer` that writes an OpenDocument text file.
pub const PRINTER_ODT: &str = "odt";
/// Value of `cpm_printer` that writes plain text.
pub const PRINTER_TEXT: &str = "text";

/// What `cpm_printer` is when nothing says otherwise.
///
/// **Off**, unlike most of the CP/M defaults. Every other setting here changes
/// how the emulator behaves inside itself; this one writes files into the
/// operator's transfer directory, and a feature that creates files unasked
/// should be asked for. With it off the `LIST` vector keeps the behaviour it
/// has always had — printer output appears on the terminal — so turning it off
/// is not a loss of function, it is a choice of where the paper goes.
pub const DEFAULT_PRINTER: &str = PRINTER_OFF;

/// The choices for `cpm_printer`, `(value, label)`.
///
/// One list for telnet, web and the desktop, the way
/// [`super::uart::UART_CHOICES`] serves the virtual modem — so the three
/// cannot offer different answers.
pub const PRINTER_CHOICES: &[(&str, &str)] = &[
    (PRINTER_OFF, "Off - printer output goes to the screen"),
    (PRINTER_ODT, "OpenDocument (.odt) in transfer printer/"),
    (PRINTER_TEXT, "Plain text (.txt) in transfer printer/"),
];

/// A printer board a *booted* disk can drive: where its data register lives.
///
/// Only the data port is needed. A real interface also has a status register
/// the guest polls for "ready", but an unclaimed port reads `0xFF` on this
/// machine and every convention in period use reads a high bit there as ready
/// — which is why Altair BASIC printed at full speed before this module
/// existed, into a board that was not there. Emulating the status register
/// would be emulating agreement we already have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrinterPort {
    /// Config value.
    pub key: &'static str,
    /// What an operator reads.
    pub label: &'static str,
    /// The port a character is written to.
    pub data: u8,
    /// Does this interface advance the paper on a bare CR?
    ///
    /// **The auto-line-feed switch**, which real Centronics-type interfaces
    /// carried as a DIP switch precisely because the byte stream cannot say:
    /// a CR that returns the carriage without advancing is how *overstrike*
    /// works, and a CR that advances is how a great deal of software ends a
    /// line. The same bytes, two meanings, settled by the hardware.
    ///
    /// So it is settled here, per board, by measurement — see
    /// [`PORT_CHOICES`]. The cost of switching it on is that overstrike becomes
    /// impossible on that printer, which is exactly the cost it had in 1977.
    pub auto_lf: bool,
}

/// Value of `cpm_printer_port` that captures nothing from a booted disk.
pub const PORT_OFF: &str = "off";

/// What `cpm_printer_port` is when nothing says otherwise.
///
/// The Altair interface, because every disk in the collections this gateway
/// boots is an Altair disk and it is the one measured against real software.
/// It costs nothing when no guest prints.
pub const DEFAULT_PRINTER_PORT: &str = "altair_c";

/// Printer boards a booted guest can be given.
///
/// **`altair_c` is measured**, by booting Altair Hard Disk BASIC (`HDSK01`),
/// answering `LINEPRINTER? C`, and watching every `OUT`: the data register is
/// `03h` and the control register `02h`. The letter `C` in that dialog is very
/// likely a Centronics-type interface, but this table names the *ports*, which
/// is what the emulation needs and what was actually observed — not a board
/// part number nothing here has verified.
///
/// **`auto_lf` is measured too**, and it had to be: two `LPRINT`s put
/// `ALPHA<CR>BETA<CR>` on the wire — a bare CR and no line feed anywhere. With
/// the switch off, `BETA` prints on top of `ALPHA` and an entire report
/// collapses onto one line. The gate is
/// `test_measure_what_altair_basic_sends_to_the_printer` in `boot_machine.rs`,
/// and it asserts the byte stream rather than describing it, so a disk that
/// ever disagrees says so instead of quietly printing nonsense.
pub const PORT_CHOICES: &[PrinterPort] = &[PrinterPort {
    key: "altair_c",
    label: "Altair line printer, data 03h (BASIC's 'C')",
    data: 0x03,
    auto_lf: true,
}];

/// What an operator reads when `cpm_printer_port` names no board.
///
/// Beside the boards rather than in each menu, for the same reason
/// [`PRINTER_CHOICES`] is one list: this label was hand-copied into the telnet
/// screen, the web page and the desktop, and three copies of a string are three
/// chances to describe the same setting differently.
pub const PORT_OFF_LABEL: &str = "No printer on a booted disk";

/// The label for a `cpm_printer` value, falling back to the `off` one.
///
/// The fallback is the point: every surface has to describe an unrecognised
/// value the way [`format_for`] *treats* it, which is as off. Doing that in
/// three places worked only by everyone remembering to.
pub fn printer_label(value: &str) -> &'static str {
    let v = value.trim();
    PRINTER_CHOICES
        .iter()
        .find(|(k, _)| *k == v)
        .map(|(_, l)| *l)
        .unwrap_or(PRINTER_CHOICES[0].1)
}

/// The label for a `cpm_printer_port` value, falling back to [`PORT_OFF_LABEL`].
pub fn port_label(value: &str) -> &'static str {
    port_for(value).map(|p| p.label).unwrap_or(PORT_OFF_LABEL)
}

/// Resolve `cpm_printer_port` to the port a booted guest prints to.
///
/// `None` for `off` and for anything unrecognised — a hand-edited typo turns
/// the capture off rather than silently selecting a port the operator did not
/// name, because a wrong port would swallow bytes meant for another device.
pub fn port_for(value: &str) -> Option<&'static PrinterPort> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case(PORT_OFF) {
        return None;
    }
    PORT_CHOICES.iter().find(|p| p.key.eq_ignore_ascii_case(v))
}

/// The output format `cpm_printer` selects, or `None` when off.
///
/// Anything unrecognised reads as **off**, the same way an unknown
/// `cpm_printer_port` does: this is hand-editable, and the failure to avoid is
/// a gateway quietly filling a folder with files in a format nobody chose.
pub fn format_for(value: &str) -> Option<Format> {
    let v = value.trim();
    if v.eq_ignore_ascii_case(PRINTER_ODT) {
        Some(Format::Odt)
    } else if v.eq_ignore_ascii_case(PRINTER_TEXT) {
        Some(Format::Text)
    } else {
        None
    }
}

/// What to leave in the transfer folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// OpenDocument text — a ZIP container, opened by LibreOffice and Word.
    Odt,
    /// Plain text, for when a document is not wanted.
    Text,
}

impl Format {
    /// The filename extension, without the dot.
    pub fn extension(&self) -> &'static str {
        match self {
            Format::Odt => "odt",
            Format::Text => "txt",
        }
    }
}

/// The folder inside the transfer directory that finished documents land in.
///
/// A folder of its own because a printer left on writes a document every time
/// anything prints, and the transfer directory is where people keep their own
/// files — a feature that quietly scatters `PRINT-*.odt` through it has made
/// itself a nuisance. It sits beside `CPM/` rather than inside it: the guest
/// must not be able to open its own printouts, and the file-transfer menu
/// reaches this one by changing directory into it.
pub const SPOOL_DIR: &str = "printer";

/// How long a printer may be silent before the job is considered finished.
///
/// There is no end-of-job signal in CP/M, so this is the whole answer for a
/// booted disk and half of it for the emulator, which also closes when the
/// program returns to `A>` (see `cpmemu_repl`). Five seconds is long enough
/// that a program pausing to read the next record does not split a document in
/// two, and short enough that a user who has finished printing does not wait
/// about for their file.
pub const IDLE_CLOSE: Duration = Duration::from_secs(5);

/// Most bytes one job may hold, after which it is closed and a new one starts.
///
/// A runaway program printing in a loop is bounded the way every other guest
/// runaway here is bounded — it does not get to fill the operator's disk. At
/// 64 columns × 66 lines this is something like 900 pages, so nothing a person
/// prints deliberately will ever reach it, and reaching it splits the document
/// rather than discarding anything.
pub const MAX_JOB_BYTES: usize = 4 * 1024 * 1024;

/// Most pages one job may hold, after which it is closed the same way
/// [`MAX_JOB_BYTES`] closes it.
///
/// A second bound because the byte bound does not imply it: a form feed is one
/// byte and a whole page, so a guest emitting nothing but `0C` reaches four
/// million *pages* before it reaches four million bytes. **Measured, not
/// feared** — a probe run to the byte bound held 4,194,305 pages, and at forty
/// bytes of bookkeeping each that is 160 MB of memory for a document with
/// nothing on it. This gateway runs on a Pi Zero.
///
/// 4096 is far above the ~900 pages the byte bound allows a real document at 64
/// columns by 66 lines, so nothing anybody prints deliberately reaches it, and a
/// job that does is closed and continued rather than truncated.
pub const MAX_JOB_PAGES: usize = 4096;

/// Columns before the printer wraps to the next line.
///
/// Wrapping at all is the choice worth noting: a real line printer at the end
/// of its carriage either wraps or truncates, and truncating would silently
/// lose text. 132 is the wide-carriage width period line printers had, and it
/// is generous enough that software formatting for 80 never reaches it.
pub const MAX_COLUMNS: usize = 132;

/// One page of captured output: a grid of characters filled by a moving head.
///
/// Modelled as a *print head over lines* rather than as a string, because that
/// is what the byte stream describes and it is the only model in which
/// overstrike means anything. A bare CR returns the head to column 0 of the
/// line it is on — it does not start a new line — which is exactly how
/// double-strike bold and underline are produced.
#[derive(Debug, Default, Clone)]
pub struct Page {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

impl Page {
    /// Put `c` at the head and advance, growing the page as needed.
    ///
    /// **Overstrike keeps the letter.** Writing where a non-space character
    /// already sits is a second pass of the head: WordStar's double-strike
    /// writes the same character again (so it makes no difference), and its
    /// underline overprints a letter with `_` — where taking the newcomer would
    /// throw away the word and leave a row of underscores. So `_` never
    /// displaces a character, and a space never erases one. Everything else
    /// wins, because a program deliberately overprinting two different letters
    /// meant the second one.
    fn put(&mut self, c: char) {
        if self.col >= MAX_COLUMNS {
            self.wrap();
        }
        while self.lines.len() <= self.row {
            self.lines.push(Vec::new());
        }
        let line = &mut self.lines[self.row];
        while line.len() <= self.col {
            line.push(' ');
        }
        let existing = line[self.col];
        let keep = existing != ' ' && (c == '_' || c == ' ');
        if !keep {
            line[self.col] = c;
        }
        self.col += 1;
    }

    /// Line feed: down one row, **column unchanged**.
    ///
    /// A real printer's LF turns the platen and does not move the carriage. It
    /// matters that these are two motions and not one: software sends CR and LF
    /// separately, in either order, and both orders land correctly only because
    /// the CR is the thing that zeroes the column.
    fn line_feed(&mut self) {
        self.row += 1;
    }

    /// Carriage return: back to column 0 of the line the head is on.
    fn carriage_return(&mut self) {
        self.col = 0;
    }

    /// Running off the end of the carriage: both motions at once.
    ///
    /// Not the same as a line feed, which is why it is not spelled with one —
    /// conflating the two put every LF at the left margin and quietly undid
    /// the model that makes overstrike work.
    fn wrap(&mut self) {
        self.row += 1;
        self.col = 0;
    }

    /// Is there nothing on this page at all?
    ///
    /// Used to drop the trailing empty page a final form feed leaves behind,
    /// which would otherwise print as a blank sheet.
    fn is_blank(&self) -> bool {
        self.lines.iter().all(|l| l.iter().all(|&c| c == ' '))
    }

    /// The page as lines of text, trailing blanks trimmed.
    fn rows(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .lines
            .iter()
            .map(|l| l.iter().collect::<String>().trim_end().to_string())
            .collect();
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        out
    }
}

/// A print job being built up.
pub struct SpoolJob {
    pages: Vec<Page>,
    bytes: usize,
    last_byte: Instant,
    /// Does this printer advance the paper on a bare CR?  See
    /// [`PrinterPort::auto_lf`].
    auto_lf: bool,
    /// Was the byte before this one a CR?  Only consulted under `auto_lf`, to
    /// absorb the LF of a CR LF pair instead of double-spacing it.
    after_cr: bool,
    /// Has a printable, non-space character been seen?
    ///
    /// Tracked as a flag rather than asked of the pages, because it is consulted
    /// at every loop seam — several hundred times a second — and re-scanning a
    /// 900-page document to answer it would be the expensive way to learn
    /// nothing new.
    content: bool,
    /// Where the last completed page ended, so `TAB` and wrapping are per page.
    tab_stop: usize,
}

impl Default for SpoolJob {
    fn default() -> Self {
        Self::new()
    }
}

impl SpoolJob {
    /// An empty job with the idle clock started, for a printer that treats CR
    /// and LF as the two separate motions they are.
    ///
    /// This is the emulator's printer: `LST:` is an operating-system service
    /// there, so what reaches it is whatever CP/M's own text convention put in
    /// the file — CR LF, which `PIP LST:=FILE.TXT` copies through verbatim.
    /// Overstrike is meaningful and works.
    ///
    /// **Measured, like the board's switch, and by the same standard.**
    /// `PIP LST:=DEMO.ASM` was run under the emulator against the CP/M 2.2
    /// distribution disk and the resulting document compared with the source
    /// file, which the guest's own `PIP` had copied out to a folder-backed
    /// drive: 2745 bytes, **65 CR and 65 LF**, 65 lines out, byte for byte
    /// identical. So CR LF is what arrives through BDOS 5 and overstrike keeps
    /// its meaning here.
    ///
    /// If a transient ever turns up that ends its printed lines with a bare CR,
    /// its document will come out as one overprinted line — and the fix is to
    /// give the emulator's printer the same switch, not to guess from the byte
    /// stream, which is exactly what the hardware could not do either.
    pub fn new() -> Self {
        SpoolJob {
            pages: vec![Page::default()],
            bytes: 0,
            last_byte: Instant::now(),
            auto_lf: false,
            after_cr: false,
            content: false,
            tab_stop: 8,
        }
    }

    /// An empty job for a printer whose auto-line-feed switch is on — a bare CR
    /// advances the paper. See [`PrinterPort::auto_lf`] for why that is a
    /// property of the board and not something the bytes can tell us.
    pub fn new_for(port: &PrinterPort) -> Self {
        SpoolJob { auto_lf: port.auto_lf, ..Self::new() }
    }

    /// Is there nothing here worth writing out?
    ///
    /// **Not "no bytes arrived".** A guest that merely *initialises* its printer
    /// has already sent bytes: Altair Hard Disk BASIC answering `LINEPRINTER? C`
    /// writes `11h` to the data port before anything is printed. Judged by bytes
    /// accepted, that lone handshake byte is a print job — and five seconds
    /// later the operator would be handed an empty document, and told about it,
    /// for the crime of turning the printer on. So this asks whether any
    /// printable, non-space character was ever seen.
    ///
    /// `len() == 0` still implies this, so the pair reads the way a caller
    /// expects; it is the converse that deliberately does not hold.
    pub fn is_empty(&self) -> bool {
        !self.content
    }

    /// Bytes accepted so far.
    pub fn len(&self) -> usize {
        self.bytes
    }

    /// Has the printer been silent long enough to call the job finished?
    ///
    /// False for an empty job: an idle printer that has printed nothing is not
    /// a finished document, and returning true would have the caller writing an
    /// empty file every five seconds forever.
    pub fn idle_expired(&self) -> bool {
        self.idle_expired_at(Instant::now())
    }

    /// [`SpoolJob::idle_expired`] against a clock the caller supplies.
    ///
    /// The seam exists so the five-second rule is *tested* rather than
    /// described: the alternative is a test that sleeps for five seconds, and a
    /// test nobody will run is the same as no test. `duration_since` saturates,
    /// so a `now` from before the last byte reads as no time passed rather than
    /// panicking.
    fn idle_expired_at(&self, now: Instant) -> bool {
        !self.is_empty() && now.duration_since(self.last_byte) >= IDLE_CLOSE
    }

    /// Is the job at either of its bounds?
    ///
    /// Two bounds, because neither implies the other: a page of text is
    /// thousands of bytes, but a form feed is one byte and a whole page.
    pub fn is_full(&self) -> bool {
        self.bytes >= MAX_JOB_BYTES || self.pages.len() >= MAX_JOB_PAGES
    }

    /// Accept one byte from the guest.
    ///
    /// Control bytes other than the four a printer acts on are **dropped, not
    /// rendered**: the guest's own driver initialisation lands here (Altair
    /// BASIC writes `11h` to the data port when it starts up), and a document
    /// beginning with a stray `DC1` would look like our bug rather than the
    /// hardware's handshake. The high bit is cleared for the same reason it is
    /// on the console path — period printers were 7-bit and software sets bit 7
    /// as a flag.
    pub fn push(&mut self, byte: u8) {
        self.last_byte = Instant::now();
        self.bytes += 1;
        let b = byte & 0x7F;
        // A space is not content: a driver that sends a line of them has still
        // printed nothing, and the page would come out blank either way.
        if (0x21..=0x7E).contains(&b) {
            self.content = true;
        }
        let after_cr = std::mem::replace(&mut self.after_cr, b == b'\r');
        let auto_lf = self.auto_lf;
        let page = self.pages.last_mut().expect("always at least one page");
        match b {
            b'\r' => {
                page.carriage_return();
                // On a printer whose auto-line-feed switch is on, the CR *is*
                // the line ending — measured: Altair BASIC's `LPRINT` sends
                // `ALPHA<CR>BETA<CR>` and nothing else, so without this the
                // second line prints on top of the first.
                if auto_lf {
                    page.line_feed();
                }
            }
            // The LF of a CR LF pair is absorbed under auto-line-feed, or the
            // paper would move twice and every document would come out
            // double-spaced.  This is what the real interfaces did, and it is
            // why the switch was usable at all by software that sent both.
            b'\n' if auto_lf && after_cr => {}
            b'\n' => page.line_feed(),
            0x0C => {
                // Form feed: this page is done.
                self.pages.push(Page::default());
            }
            b'\t' => {
                let next = (page.col / self.tab_stop + 1) * self.tab_stop;
                for _ in page.col..next.min(MAX_COLUMNS) {
                    page.put(' ');
                }
            }
            0x08 => page.col = page.col.saturating_sub(1),
            0x20..=0x7E => page.put(b as char),
            // Everything else — NUL, the DC handshake bytes, BEL, ESC and any
            // escape sequence's payload — is not text and not an action we
            // model.  Dropped silently rather than logged: a driver that sends
            // one per character would fill the log with its own normality.
            _ => {}
        }
    }

    /// Render and write the job into `transfer_dir/printer`, returning the name
    /// as the operator will see it — `printer/PRINT-…`, path and all.
    ///
    /// Under the **transfer directory**, deliberately, and not anywhere under
    /// `CPM/`: this file is for the person at the gateway, and putting it on a
    /// CP/M drive would both hand it back to the guest as a file it could open
    /// and hide it from the file-transfer menu, which is how the operator is
    /// meant to collect it. In a [`SPOOL_DIR`] subfolder of its own rather than
    /// the root, because a printer that is left on produces a document every
    /// time anything prints, and a transfer directory is somewhere people keep
    /// their own files.
    ///
    /// Written to a temporary name and renamed into place, the same rule whole
    /// image writes follow — a reader who lists the folder while a long job is
    /// being rendered must not find a half-written document that looks
    /// finished.
    ///
    /// **Two sessions can print at once**, and the timestamp only resolves to a
    /// second, so neither name may be assumed unique. The staging name carries a
    /// process-wide counter so two jobs cannot write over each other's
    /// half-finished bytes, and the final name is probed for a free one — which
    /// also matters because `fs::rename` **fails on Windows** when the target
    /// exists rather than replacing it, so relying on overwrite would have made
    /// a same-second collision an error on one platform and a lost document on
    /// the others.
    pub fn write(&self, transfer_dir: &str, format: Format) -> std::io::Result<String> {
        let dir = Path::new(transfer_dir).join(SPOOL_DIR);
        std::fs::create_dir_all(&dir)?;
        let (stem, ext) = self.file_stem_and_ext(format);
        let body = match format {
            Format::Odt => build_odt(&self.pages),
            Format::Text => self.plain_text().into_bytes(),
        };

        // Unique per call, so concurrent jobs cannot share a staging file.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let staged = dir.join(format!(".{stem}.{}.{seq}.part", std::process::id()));
        std::fs::write(&staged, &body)?;

        // The first free name: `PRINT-…`, then `PRINT-…-2` and so on. Bounded
        // rather than looping forever if the folder is somehow unwritable.
        let mut name = format!("{stem}.{ext}");
        for n in 2..=99 {
            if !dir.join(&name).exists() {
                break;
            }
            name = format!("{stem}-{n}.{ext}");
        }
        if let Err(e) = std::fs::rename(&staged, dir.join(&name)) {
            // Do not leave the staging file behind to be collected by a puzzled
            // operator as though it were their document.
            let _ = std::fs::remove_file(&staged);
            return Err(e);
        }
        // Reported with the folder in front of it.  The operator has to go
        // somewhere to fetch this, and a bare file name would send them looking
        // in the transfer root where it is not.
        Ok(format!("{SPOOL_DIR}/{name}"))
    }

    /// `("PRINT-YYYYMMDD-HHMMSS", "odt")` from the host's own clock, without the
    /// folder — [`SpoolJob::write`] puts that in front of what it returns, and
    /// keeps the two apart so it can disambiguate a collision in the middle.
    ///
    /// A timestamp rather than a counter because a counter has to remember, and
    /// the thing it would have to remember lives across restarts. Seconds are
    /// enough to name a job; they are **not** enough to be unique, which is why
    /// the caller probes — two sessions printing at once is ordinary, and the
    /// idle close makes them likelier to finish together, not less.
    fn file_stem_and_ext(&self, format: Format) -> (String, &'static str) {
        let (y, m, d, h, mi, s) = super::host_clock_parts();
        (
            format!("PRINT-{y:04}{m:02}{d:02}-{h:02}{mi:02}{s:02}"),
            format.extension(),
        )
    }

    /// The job as plain text, pages separated by a form feed.
    ///
    /// The form feed is kept rather than dropped, because in a text file it is
    /// the only remaining record of where the guest broke the page, and `lpr`
    /// and every terminal pager act on it.
    ///
    /// Crate-visible so the live measurement gate in `boot_machine.rs` can push
    /// a real disk's real bytes through the real spool and compare the document
    /// — checking the byte stream and then *describing* what the page model
    /// would do with it is how a plausible-but-wrong model survives.
    pub(crate) fn plain_text(&self) -> String {
        let mut out = String::new();
        for (i, page) in self.live_pages().iter().enumerate() {
            if i > 0 {
                out.push('\u{0C}');
            }
            for row in page.rows() {
                out.push_str(&row);
                out.push('\n');
            }
        }
        out
    }

    /// The pages worth writing: all of them, less a blank one at the end.
    fn live_pages(&self) -> Vec<&Page> {
        live_pages(&self.pages)
    }
}

/// Drop the empty page a trailing form feed leaves behind, which would
/// otherwise come out as a blank final sheet.
///
/// A free function because *both* output paths need it and only one of them
/// goes through a [`SpoolJob`] — [`build_odt`] takes bare pages. This rule was
/// written out twice for a while, which is precisely the shape of duplication
/// this project has been bitten by: the two copies can disagree, and a document
/// with one more sheet than the text file beside it is a bug nobody would look
/// for here. Never fewer than one page: a job that printed nothing but a form
/// feed is still a sheet of paper.
fn live_pages(pages: &[Page]) -> Vec<&Page> {
    let mut live: Vec<&Page> = pages.iter().collect();
    while live.len() > 1 && live.last().is_some_and(|p| p.is_blank()) {
        live.pop();
    }
    live
}

// =============================================================================
// OpenDocument text
// =============================================================================

/// The `mimetype` an ODF text document declares.
const ODT_MIME: &str = "application/vnd.oasis.opendocument.text";

/// Append one character, escaped if XML will not take it literally.
///
/// A character at a time rather than a string at a time, because the caller has
/// characters: it is walking the line to count runs of spaces anyway, and the
/// string form meant allocating two `String`s per character of the document —
/// millions of them at the size bound, to escape five characters that almost
/// never appear.
fn push_escaped(out: &mut String, c: char) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' => out.push_str("&quot;"),
        '\'' => out.push_str("&apos;"),
        _ => out.push(c),
    }
}

/// One paragraph of printer output.
///
/// Leading spaces are the *layout* on a line printer, and XML collapses runs of
/// whitespace — so they go out as `<text:s text:c="n"/>`, which is ODF's way of
/// saying "n spaces, and mean it". Without this every indented line, every
/// column of every table a program printed, slides left against the margin.
fn odt_paragraph(style: &str, row: &str) -> String {
    if row.is_empty() {
        return format!("<text:p text:style-name=\"{style}\"/>");
    }
    let lead = row.len() - row.trim_start().len();
    let mut body = String::new();
    if lead > 0 {
        body.push_str(&format!("<text:s text:c=\"{lead}\"/>"));
    }
    // Interior runs of two or more spaces need the same treatment; a single
    // space between words is safe and reads better in the XML.
    let rest = &row[lead..];
    let mut run = 0usize;
    for c in rest.chars() {
        if c == ' ' {
            run += 1;
            continue;
        }
        if run == 1 {
            body.push(' ');
        } else if run > 1 {
            body.push_str(&format!("<text:s text:c=\"{run}\"/>"));
        }
        run = 0;
        push_escaped(&mut body, c);
    }
    format!("<text:p text:style-name=\"{style}\">{body}</text:p>")
}

/// `content.xml` for the captured pages.
///
/// Monospace, because the guest laid its output out in columns and a
/// proportional font destroys that. Two paragraph styles: the ordinary one, and
/// one that breaks the page before it, applied to the first line of every page
/// after the first — that is how a form feed survives into a document that is
/// going to be printed again at the other end.
fn odt_content(pages: &[&Page]) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <office:document-content \
         xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
         xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
         xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" \
         xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" \
         office:version=\"1.3\">\
         <office:automatic-styles>\
         <style:style style:name=\"LP\" style:family=\"paragraph\">\
         <style:text-properties style:font-name-complex=\"monospace\" \
         fo:font-family=\"monospace\" fo:font-size=\"10pt\"/>\
         <style:paragraph-properties fo:margin-top=\"0cm\" fo:margin-bottom=\"0cm\"/>\
         </style:style>\
         <style:style style:name=\"LPB\" style:family=\"paragraph\" \
         style:parent-style-name=\"LP\">\
         <style:text-properties style:font-name-complex=\"monospace\" \
         fo:font-family=\"monospace\" fo:font-size=\"10pt\"/>\
         <style:paragraph-properties fo:break-before=\"page\" \
         fo:margin-top=\"0cm\" fo:margin-bottom=\"0cm\"/>\
         </style:style>\
         </office:automatic-styles>\
         <office:body><office:text>",
    );
    for (i, page) in pages.iter().enumerate() {
        let rows = page.rows();
        // A page with nothing on it still has to occupy a sheet, or a
        // deliberately blank page in the middle of a report disappears.
        if rows.is_empty() {
            xml.push_str(&odt_paragraph(if i == 0 { "LP" } else { "LPB" }, ""));
            continue;
        }
        for (j, row) in rows.iter().enumerate() {
            let style = if i > 0 && j == 0 { "LPB" } else { "LP" };
            xml.push_str(&odt_paragraph(style, row));
        }
    }
    xml.push_str("</office:text></office:body></office:document-content>");
    xml
}

/// `META-INF/manifest.xml` listing what the package holds.
fn odt_manifest() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <manifest:manifest \
         xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\" \
         manifest:version=\"1.3\">\
         <manifest:file-entry manifest:full-path=\"/\" \
         manifest:media-type=\"{ODT_MIME}\"/>\
         <manifest:file-entry manifest:full-path=\"content.xml\" \
         manifest:media-type=\"text/xml\"/>\
         </manifest:manifest>"
    )
}

/// Build the whole `.odt`: a ZIP holding `mimetype`, the manifest and content.
///
/// **`mimetype` must be the first entry and must be stored uncompressed**, per
/// the ODF packaging rules — it is there so a reader can identify the format by
/// reading the first bytes of the file, which only works if it is neither moved
/// nor deflated. Everything here is stored rather than deflated: it costs some
/// size on a document that is mostly spaces, and it means this module needs no
/// compressor at all.
pub fn build_odt(pages: &[Page]) -> Vec<u8> {
    let live = live_pages(pages);
    let mut zip = Zip::default();
    zip.add("mimetype", ODT_MIME.as_bytes());
    zip.add("META-INF/manifest.xml", odt_manifest().as_bytes());
    zip.add("content.xml", odt_content(&live).as_bytes());
    zip.finish()
}

/// A minimal stored-only ZIP writer.
///
/// Stored (method 0) only, which is what makes this short enough to own: the
/// CRC-32 is the one already in [`crate::zmodem`] — ZIP and ZMODEM use the same
/// reflected `0xEDB88320` polynomial — and the compressed size is the
/// uncompressed size. No deflate, no bit packing, nothing to get subtly wrong.
#[derive(Default)]
struct Zip {
    out: Vec<u8>,
    entries: Vec<ZipEntry>,
}

struct ZipEntry {
    name: String,
    crc: u32,
    len: u32,
    offset: u32,
}

impl Zip {
    /// Append one stored entry.
    fn add(&mut self, name: &str, data: &[u8]) {
        let offset = self.out.len() as u32;
        let crc = crate::zmodem::crc32(data);
        let len = data.len() as u32;
        self.out.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]); // local header
        self.out.extend_from_slice(&10u16.to_le_bytes()); // version needed
        self.out.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        self.out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        self.out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.out.extend_from_slice(&len.to_le_bytes()); // compressed
        self.out.extend_from_slice(&len.to_le_bytes()); // uncompressed
        self.out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // extra field length
        self.out.extend_from_slice(name.as_bytes());
        self.out.extend_from_slice(data);
        self.entries.push(ZipEntry { name: name.to_string(), crc, len, offset });
    }

    /// Central directory + end record, returning the finished archive.
    fn finish(mut self) -> Vec<u8> {
        let cd_start = self.out.len() as u32;
        for e in &self.entries {
            self.out.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]); // central header
            self.out.extend_from_slice(&20u16.to_le_bytes()); // version made by
            self.out.extend_from_slice(&10u16.to_le_bytes()); // version needed
            self.out.extend_from_slice(&0u16.to_le_bytes()); // flags
            self.out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            self.out.extend_from_slice(&0u16.to_le_bytes()); // mod time
            self.out.extend_from_slice(&0u16.to_le_bytes()); // mod date
            self.out.extend_from_slice(&e.crc.to_le_bytes());
            self.out.extend_from_slice(&e.len.to_le_bytes());
            self.out.extend_from_slice(&e.len.to_le_bytes());
            self.out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            self.out.extend_from_slice(&0u16.to_le_bytes()); // extra
            self.out.extend_from_slice(&0u16.to_le_bytes()); // comment
            self.out.extend_from_slice(&0u16.to_le_bytes()); // disk number
            self.out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            self.out.extend_from_slice(&e.offset.to_le_bytes());
            self.out.extend_from_slice(e.name.as_bytes());
        }
        let cd_len = self.out.len() as u32 - cd_start;
        let count = self.entries.len() as u16;
        self.out.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]); // end of central dir
        self.out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        self.out.extend_from_slice(&0u16.to_le_bytes()); // disk with CD
        self.out.extend_from_slice(&count.to_le_bytes());
        self.out.extend_from_slice(&count.to_le_bytes());
        self.out.extend_from_slice(&cd_len.to_le_bytes());
        self.out.extend_from_slice(&cd_start.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        self.out
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a byte string through a fresh job and return its plain text.
    ///
    /// Every page test goes through the real `push`, not through `Page`
    /// directly: the control-byte handling and the page model are one behaviour
    /// as far as a caller is concerned, and testing the halves separately would
    /// let a byte be routed to the wrong one without a test noticing.
    fn text_of(bytes: &[u8]) -> String {
        let mut job = SpoolJob::new();
        for &b in bytes {
            job.push(b);
        }
        job.plain_text()
    }

    /// The rows of the first page, which is what most of these are about.
    fn rows_of(bytes: &[u8]) -> Vec<String> {
        let mut job = SpoolJob::new();
        for &b in bytes {
            job.push(b);
        }
        job.pages[0].rows()
    }

    // ── the config values ────────────────────────────────────────────────────

    #[test]
    fn test_format_for_reads_the_three_settings() {
        assert_eq!(format_for(PRINTER_ODT), Some(Format::Odt));
        assert_eq!(format_for(PRINTER_TEXT), Some(Format::Text));
        assert_eq!(format_for(PRINTER_OFF), None);
        // Hand-edited config files: surrounding space and odd case must not
        // silently turn a printer off.
        assert_eq!(format_for("  odt  "), Some(Format::Odt));
        assert_eq!(format_for("ODT"), Some(Format::Odt));
        assert_eq!(format_for("Text"), Some(Format::Text));
        // Anything else is off, which is the safe direction: the failure to
        // avoid is a gateway filling a folder with files nobody asked for.
        assert_eq!(format_for(""), None);
        assert_eq!(format_for("pdf"), None);
        assert_eq!(format_for("odt2"), None);
    }

    #[test]
    fn test_port_for_resolves_only_the_named_boards() {
        assert_eq!(port_for("altair_c").map(|p| p.data), Some(0x03));
        assert_eq!(port_for("  ALTAIR_C ").map(|p| p.data), Some(0x03));
        assert!(port_for(PORT_OFF).is_none());
        assert!(port_for("").is_none());
        // A typo must not select a port the operator did not name — a wrong
        // port would swallow bytes meant for another device.
        assert!(port_for("altair").is_none());
        assert!(port_for("0x03").is_none());
    }

    /// The defaults have to be values the resolvers actually accept, and the
    /// menus have to offer values the config parser accepts.  Three surfaces
    /// read these lists; a value in one and not the others is the exact defect
    /// this project has shipped before.
    #[test]
    fn test_defaults_and_choices_agree_with_the_resolvers() {
        assert!(
            PRINTER_CHOICES.iter().any(|(v, _)| *v == DEFAULT_PRINTER),
            "the default printer setting is not one of the offered choices"
        );
        assert_eq!(DEFAULT_PRINTER, PRINTER_OFF, "printing must be off unless asked for");
        assert!(
            port_for(DEFAULT_PRINTER_PORT).is_some(),
            "the default printer port does not resolve to a board"
        );
        for (value, label) in PRINTER_CHOICES {
            assert_eq!(
                format_for(value).is_some(),
                *value != PRINTER_OFF,
                "the choice {value:?} does not resolve the way its list says"
            );
            assert!(!label.is_empty(), "{value:?} has no label for the menus");
        }
        for p in PORT_CHOICES {
            assert_eq!(port_for(p.key).map(|q| q.data), Some(p.data));
            assert_ne!(p.key, PORT_OFF, "a board may not be named `off`");
        }
        // Every board's data port must be one no console or modem profile also
        // answers: the printer is a port we volunteer to claim, and a collision
        // would break the guest's own hardware rather than merely lose a
        // printout.  `03h` is clear of the 88-SIO (00/01), the 88-2SIO
        // (10h-13h) and every RC2014 profile (80h-87h).
        for p in PORT_CHOICES {
            assert!(
                !matches!(p.data, 0x00 | 0x01 | 0x10..=0x13 | 0x80..=0x87),
                "{} claims {:#04x}, which a console or modem profile already answers",
                p.key,
                p.data
            );
        }
    }

    // ── the page model ───────────────────────────────────────────────────────

    #[test]
    fn test_plain_characters_land_in_order() {
        assert_eq!(rows_of(b"HELLO"), vec!["HELLO"]);
    }

    /// A bare CR returns the head to column 0 of the line it is on.  It does
    /// *not* start a new line — which is the whole basis of overstrike, and the
    /// single most important thing in this module to get right.
    #[test]
    fn test_bare_cr_returns_the_head_without_advancing() {
        assert_eq!(rows_of(b"AB\rC"), vec!["CB"]);
    }

    /// LF moves down and leaves the column alone, like a real printer's platen.
    /// Software that sends CR and LF in either order still lands correctly,
    /// because the CR is what zeroes the column.
    #[test]
    fn test_lf_moves_down_without_returning_the_carriage() {
        assert_eq!(rows_of(b"AB\nC"), vec!["AB", "  C"]);
        assert_eq!(rows_of(b"AB\r\nC"), vec!["AB", "C"]);
        assert_eq!(rows_of(b"AB\n\rC"), vec!["AB", "C"]);
    }

    // ── the auto-line-feed switch ────────────────────────────────────────────

    fn rows_auto(bytes: &[u8]) -> Vec<String> {
        let board = port_for("altair_c").expect("the Altair board");
        assert!(board.auto_lf, "this helper is about the switch being on");
        let mut job = SpoolJob::new_for(board);
        for &b in bytes {
            job.push(b);
        }
        job.pages[0].rows()
    }

    /// **The measured case.** Two `LPRINT`s from Altair Hard Disk BASIC put
    /// `ALPHA<CR>BETA<CR>` on the wire and nothing else — no line feed anywhere.
    /// On a printer with the switch off that is one line with `BETA` printed
    /// over `ALPHA`; a whole report collapses. The gate that measured it is
    /// `test_measure_what_altair_basic_sends_to_the_printer`.
    #[test]
    fn test_auto_line_feed_makes_a_bare_cr_end_the_line() {
        assert_eq!(rows_auto(b"ALPHA\rBETA\r"), vec!["ALPHA", "BETA"]);
        // And the switch off is the other behaviour, which is the whole reason
        // it is a switch: `BETA` lands on top of `ALPHA`.
        assert_eq!(rows_of(b"ALPHA\rBETA\r"), vec!["BETAA"]);
    }

    /// A printer with the switch on that is *also* sent CR LF must not
    /// double-space — the LF of the pair is absorbed, which is what the real
    /// interfaces did and what made the switch usable at all.
    #[test]
    fn test_auto_line_feed_absorbs_the_lf_of_a_cr_lf_pair() {
        assert_eq!(rows_auto(b"ONE\r\nTWO\r\n"), vec!["ONE", "TWO"]);
        // Only the LF *immediately* after the CR is absorbed: a deliberate
        // blank line is still a blank line.
        assert_eq!(rows_auto(b"ONE\r\n\nTWO\r\n"), vec!["ONE", "", "TWO"]);
    }

    /// The switch is a property of the board, so the two constructors have to
    /// disagree — if `new_for` ever stopped reading it, every test above would
    /// still pass while a booted disk printed nonsense.
    #[test]
    fn test_a_job_takes_the_switch_from_its_board() {
        let board = port_for("altair_c").expect("the Altair board");
        assert!(SpoolJob::new_for(board).auto_lf, "the board's switch was not read");
        assert!(!SpoolJob::new().auto_lf, "the OS-service printer must not auto-feed");
    }

    /// WordStar underlines by overprinting a letter with `_`.  Taking the
    /// newcomer would throw the word away and leave a row of underscores.
    #[test]
    fn test_underscore_overstrike_keeps_the_letter() {
        assert_eq!(rows_of(b"WORD\r____"), vec!["WORD"]);
        // The other order is a letter arriving over an underscore, and there
        // the letter is still the thing worth keeping.
        assert_eq!(rows_of(b"____\rWORD"), vec!["WORD"]);
    }

    #[test]
    fn test_a_space_never_erases_a_character() {
        assert_eq!(rows_of(b"AB\r  "), vec!["AB"]);
    }

    /// Double-strike bold prints the line twice.  The result must be the line,
    /// not the line doubled.
    #[test]
    fn test_double_strike_is_not_doubled() {
        assert_eq!(rows_of(b"BOLD\rBOLD"), vec!["BOLD"]);
    }

    /// Two *different* letters overprinted means the program meant the second.
    #[test]
    fn test_a_different_character_wins_the_column() {
        assert_eq!(rows_of(b"AB\rXY"), vec!["XY"]);
    }

    #[test]
    fn test_tab_advances_to_the_next_eight_column_stop() {
        assert_eq!(rows_of(b"AB\tC"), vec!["AB      C"]);
        // Already on a stop: a tab moves a full eight, it does not stand still.
        assert_eq!(rows_of(b"AB\t\tC"), vec!["AB              C"]);
        assert_eq!(rows_of(b"\tX"), vec!["        X"]);
    }

    /// Tabbing across text must not wipe it — the tab writes spaces, and a
    /// space never erases.
    #[test]
    fn test_tab_over_existing_text_keeps_it() {
        assert_eq!(rows_of(b"ABCDEFGH\rX\tY"), vec!["XBCDEFGHY"]);
    }

    #[test]
    fn test_backspace_moves_the_head_left() {
        assert_eq!(rows_of(b"AB\x08C"), vec!["AC"]);
        // At the left margin there is nowhere to go, and it must not underflow.
        assert_eq!(rows_of(b"\x08A"), vec!["A"]);
    }

    #[test]
    fn test_form_feed_starts_a_new_page() {
        assert_eq!(text_of(b"ONE\x0CTWO"), "ONE\n\u{0C}TWO\n");
    }

    /// A final form feed is how software *ends* a print, not how it asks for a
    /// blank sheet.  Keeping the empty page would put one at the end of every
    /// document ever printed here.
    #[test]
    fn test_a_trailing_form_feed_leaves_no_blank_page() {
        assert_eq!(text_of(b"ONE\x0C"), "ONE\n");
    }

    /// A deliberately blank page *in the middle* is content and must survive —
    /// the trimming rule is about the end of the job only.
    #[test]
    fn test_a_blank_page_in_the_middle_survives() {
        assert_eq!(text_of(b"ONE\x0C\x0CTHREE"), "ONE\n\u{0C}\u{0C}THREE\n");
    }

    /// The guest's own driver handshake lands here as data.  It is not text and
    /// must not appear in the document — a report beginning with a stray DC1
    /// would look like our bug rather than the hardware's normality.
    #[test]
    fn test_control_bytes_are_dropped_not_rendered() {
        // 0x11 is the byte Altair BASIC writes to the data port at startup.
        assert_eq!(rows_of(b"\x00\x11\x07\x1B[0mA"), vec!["[0mA"]);
    }

    /// Period printers were 7-bit and software sets bit 7 as a flag.
    #[test]
    fn test_the_high_bit_is_cleared() {
        assert_eq!(rows_of(&[0xC1, 0xC2]), vec!["AB"]);
    }

    /// A real carriage at the end of its travel wraps or truncates; truncating
    /// would silently lose text, so this wraps.
    #[test]
    fn test_the_head_wraps_at_the_carriage_width() {
        let rows = rows_of(&[b'X'; MAX_COLUMNS + 1]);
        assert_eq!(rows.len(), 2, "expected a wrap onto a second line");
        assert_eq!(rows[0].len(), MAX_COLUMNS);
        assert_eq!(rows[1], "X");
    }

    #[test]
    fn test_trailing_blank_lines_are_trimmed() {
        assert_eq!(text_of(b"A\n\n\n\n"), "A\n");
    }

    // ── when a job is over ───────────────────────────────────────────────────

    /// The five-second rule, tested against an injected clock rather than by
    /// sleeping for five seconds.
    #[test]
    fn test_idle_close_fires_only_after_the_quiet_period() {
        let mut job = SpoolJob::new();
        job.push(b'A');
        let t = job.last_byte;
        assert!(!job.idle_expired_at(t), "a job is not over the instant it starts");
        assert!(
            !job.idle_expired_at(t + IDLE_CLOSE - Duration::from_millis(1)),
            "closed a moment early — a program pausing to read a record would be split in two"
        );
        assert!(job.idle_expired_at(t + IDLE_CLOSE), "the quiet period elapsed and nothing closed");
    }

    /// An idle printer that has printed nothing is not a finished document.
    /// Returning true here would write an empty file every five seconds forever.
    #[test]
    fn test_an_empty_job_never_expires() {
        let job = SpoolJob::new();
        assert!(job.is_empty());
        assert!(!job.idle_expired_at(job.last_byte + IDLE_CLOSE * 100));
    }

    /// **The Altair handshake.** `LINEPRINTER? C` writes `11h` to the data port
    /// before a character is printed.  If that made a job, the operator would be
    /// handed an empty document — and told about it — for turning the printer
    /// on.  Bytes arrived; nothing was printed.
    #[test]
    fn test_a_driver_handshake_alone_is_not_a_print_job() {
        let mut job = SpoolJob::new();
        job.push(0x11);
        assert_eq!(job.len(), 1, "the byte was accepted");
        assert!(job.is_empty(), "a handshake byte must not count as a document");
        assert!(!job.idle_expired_at(job.last_byte + IDLE_CLOSE * 100));
    }

    /// Nor is whitespace: a driver sending a line of spaces and a form feed has
    /// still printed nothing, and the sheet would come out blank either way.
    #[test]
    fn test_whitespace_alone_is_not_a_print_job() {
        let mut job = SpoolJob::new();
        for &b in b"    \r\n\x0C" {
            job.push(b);
        }
        assert!(job.is_empty());
    }

    #[test]
    fn test_is_full_at_the_size_bound() {
        let mut job = SpoolJob::new();
        assert!(!job.is_full());
        // CR is the cheapest byte to repeat: it is counted but allocates
        // nothing, so this bounds-checks 4 MB without building a 4 MB page.
        job.push(b'A');
        for _ in 1..MAX_JOB_BYTES {
            job.push(b'\r');
        }
        assert!(job.is_full(), "the job never reached its bound");
        assert!(!job.is_empty(), "it did print something");
    }

    /// **The page bound, which the byte bound does not imply.** A form feed is
    /// one byte and a whole page, so a guest emitting nothing but `0C` reaches
    /// four million pages before four million bytes — measured at 4,194,305
    /// pages, about 160 MB of bookkeeping for a document with nothing on it, on
    /// a gateway that runs on a Pi Zero.
    #[test]
    fn test_is_full_at_the_page_bound() {
        let mut job = SpoolJob::new();
        for _ in 0..MAX_JOB_PAGES {
            job.push(0x0C);
        }
        assert!(job.is_full(), "a form-feed runaway is not bounded by page count");
        assert!(
            job.len() < MAX_JOB_BYTES,
            "this must bind long before the byte bound, or it is not doing anything"
        );
        assert!(job.pages.len() <= MAX_JOB_PAGES + 1, "held {} pages", job.pages.len());
        // And it is still not a document: nothing printable was ever sent, so
        // the close path drops it rather than writing a stack of blank sheets.
        assert!(job.is_empty());
    }

    // ── writing the file out ─────────────────────────────────────────────────

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("egw_printer_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Documents land in a subfolder, not loose in the transfer directory, and
    /// the name handed back says so — an operator told `PRINT-….txt` would go
    /// looking in the root, where it is not.
    #[test]
    fn test_write_names_the_file_and_leaves_nothing_partial() {
        let dir = temp_dir("write");
        let mut job = SpoolJob::new();
        for &b in b"REPORT\n" {
            job.push(b);
        }
        let name = job.write(dir.to_str().unwrap(), Format::Text).expect("write");

        let prefix = format!("{SPOOL_DIR}/");
        assert!(name.starts_with(&prefix), "{name} is not in the spool folder");
        let bare = &name[prefix.len()..];
        assert!(bare.starts_with("PRINT-"), "{name} is not the documented name");
        assert!(bare.ends_with(".txt"), "{name} has the wrong extension");
        // PRINT- + 8 digits + - + 6 digits + .txt
        assert_eq!(bare.len(), "PRINT-YYYYMMDD-HHMMSS.txt".len(), "{name} is the wrong shape");
        assert!(
            bare["PRINT-".len().."PRINT-YYYYMMDD-HHMMSS".len()]
                .chars()
                .all(|c| c.is_ascii_digit() || c == '-'),
            "{name} has something other than a timestamp in it"
        );

        assert_eq!(std::fs::read_to_string(dir.join(&name)).unwrap(), "REPORT\n");
        // Nothing loose in the transfer directory itself: the whole point of
        // the subfolder is that the operator's own files stay uncluttered.
        let loose: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| *n != SPOOL_DIR)
            .collect();
        assert!(loose.is_empty(), "left {loose:?} in the transfer root");
        // The staging file is renamed into place, never left behind: a reader
        // listing the folder mid-render must not find a half-written document
        // that looks finished.
        let leftovers: Vec<String> = std::fs::read_dir(dir.join(SPOOL_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| *n != bare)
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?} behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two jobs finishing in the same second must both survive.  The timestamp
    /// resolves to a second and two sessions printing at once is ordinary, so
    /// the second document has to get a name of its own rather than overwrite
    /// the first — and on Windows the plain rename would not even overwrite, it
    /// would fail.
    #[test]
    fn test_two_jobs_in_the_same_second_both_survive() {
        let dir = temp_dir("collide");
        let mut first = SpoolJob::new();
        for &b in b"FIRST\r\n" {
            first.push(b);
        }
        let mut second = SpoolJob::new();
        for &b in b"SECOND\r\n" {
            second.push(b);
        }
        let a = first.write(dir.to_str().unwrap(), Format::Text).expect("first");
        let b = second.write(dir.to_str().unwrap(), Format::Text).expect("second");

        assert_ne!(a, b, "the second job overwrote the first");
        assert_eq!(std::fs::read_to_string(dir.join(&a)).unwrap(), "FIRST\n");
        assert_eq!(std::fs::read_to_string(dir.join(&b)).unwrap(), "SECOND\n");
        // And nothing half-written was left lying about under either name.
        let leftovers: Vec<String> = std::fs::read_dir(dir.join(SPOOL_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?} behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_creates_the_folders_it_needs() {
        let root = temp_dir("mkdir");
        let dir = root.join("not").join("there");
        let mut job = SpoolJob::new();
        job.push(b'X');
        let name = job.write(dir.to_str().unwrap(), Format::Text).expect("write");
        assert!(dir.join(SPOOL_DIR).is_dir(), "the spool folder was not created");
        assert!(dir.join(name).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_extensions_match_the_formats() {
        assert_eq!(Format::Odt.extension(), "odt");
        assert_eq!(Format::Text.extension(), "txt");
    }

    // ── OpenDocument ─────────────────────────────────────────────────────────

    #[test]
    fn test_xml_escape_covers_the_five() {
        let mut out = String::new();
        for c in "a&b<c>d\"e'f".chars() {
            push_escaped(&mut out, c);
        }
        assert_eq!(out, "a&amp;b&lt;c&gt;d&quot;e&apos;f");
        let mut plain = String::new();
        for c in "plain".chars() {
            push_escaped(&mut plain, c);
        }
        assert_eq!(plain, "plain");
    }

    /// XML collapses runs of whitespace, so leading and interior spaces go out
    /// as `<text:s>`.  Without this every indented line and every column of
    /// every table a program printed slides left against the margin.
    #[test]
    fn test_odt_paragraph_preserves_layout_spaces() {
        assert_eq!(
            odt_paragraph("LP", "    X"),
            "<text:p text:style-name=\"LP\"><text:s text:c=\"4\"/>X</text:p>"
        );
        // A single space between words is safe as a literal.
        assert!(odt_paragraph("LP", "A B").contains(">A B<"));
        // Two or more are layout and must be counted.
        assert!(odt_paragraph("LP", "A   B").contains("<text:s text:c=\"3\"/>"));
        // An empty row is still a line of the document.
        assert_eq!(odt_paragraph("LP", ""), "<text:p text:style-name=\"LP\"/>");
        // And the escaping still applies inside a paragraph.
        assert!(odt_paragraph("LP", "a<b").contains("a&lt;b"));
    }

    /// A form feed has to survive into a document that will be printed again at
    /// the other end, which in ODF means a page break on the first paragraph of
    /// every page after the first.
    #[test]
    fn test_odt_breaks_the_page_at_each_form_feed() {
        let mut job = SpoolJob::new();
        for &b in b"ONE\x0CTWO\x0CTHREE" {
            job.push(b);
        }
        let xml = odt_content(&job.live_pages());
        assert_eq!(xml.matches("style-name=\"LPB\"").count(), 2, "expected two page breaks");
        assert!(xml.contains(">ONE<") && xml.contains(">TWO<") && xml.contains(">THREE<"));
    }

    /// **`mimetype` must be the first entry and must be stored uncompressed**,
    /// per the ODF packaging rules — a reader identifies the format by reading
    /// the first bytes of the file, which only works if it is neither moved nor
    /// deflated.
    #[test]
    fn test_odt_is_a_zip_whose_first_entry_is_the_stored_mimetype() {
        let mut job = SpoolJob::new();
        for &b in b"HELLO" {
            job.push(b);
        }
        let zip = build_odt(&job.pages);

        assert_eq!(&zip[0..4], &[0x50, 0x4B, 0x03, 0x04], "not a ZIP local header");
        let method = u16::from_le_bytes([zip[8], zip[9]]);
        assert_eq!(method, 0, "the mimetype entry is not stored");
        let name_len = u16::from_le_bytes([zip[26], zip[27]]) as usize;
        let extra_len = u16::from_le_bytes([zip[28], zip[29]]) as usize;
        assert_eq!(&zip[30..30 + name_len], b"mimetype", "mimetype is not the first entry");
        let data = 30 + name_len + extra_len;
        assert_eq!(&zip[data..data + ODT_MIME.len()], ODT_MIME.as_bytes());

        // The end-of-central-directory record must account for all three
        // entries, or a reader rejects the file before it reads any of them.
        let eocd = zip
            .windows(4)
            .rposition(|w| w == [0x50, 0x4B, 0x05, 0x06])
            .expect("no end-of-central-directory record");
        assert_eq!(u16::from_le_bytes([zip[eocd + 8], zip[eocd + 9]]), 3, "wrong entry count");
        assert_eq!(u16::from_le_bytes([zip[eocd + 10], zip[eocd + 11]]), 3);
    }

    /// ZIP's checksum is the standard CRC-32, and we borrow ZMODEM's — same
    /// reflected `0xEDB88320` polynomial.  If that ever stopped being true every
    /// document this module writes would be rejected as corrupt, so the shared
    /// function is pinned here against the canonical vector rather than trusted.
    #[test]
    fn test_the_zip_checksum_is_the_standard_crc32() {
        assert_eq!(crate::zmodem::crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crate::zmodem::crc32(b""), 0);
    }

    /// The central directory has to describe the local entries exactly — same
    /// name, same CRC, same length, and an offset that really is where the local
    /// header sits.  A reader trusts the directory and seeks by it.
    #[test]
    fn test_the_central_directory_points_at_the_local_headers() {
        let mut zip = Zip::default();
        zip.add("one.txt", b"first");
        zip.add("two.txt", b"second");
        let entries: Vec<(String, u32, u32, u32)> =
            zip.entries.iter().map(|e| (e.name.clone(), e.crc, e.len, e.offset)).collect();
        let out = zip.finish();
        for (name, crc, len, offset) in entries {
            let at = offset as usize;
            assert_eq!(&out[at..at + 4], &[0x50, 0x4B, 0x03, 0x04], "{name}: offset is not a header");
            assert_eq!(u32::from_le_bytes(out[at + 14..at + 18].try_into().unwrap()), crc);
            assert_eq!(u32::from_le_bytes(out[at + 18..at + 22].try_into().unwrap()), len);
            let name_len = u16::from_le_bytes([out[at + 26], out[at + 27]]) as usize;
            assert_eq!(&out[at + 30..at + 30 + name_len], name.as_bytes());
        }
    }

    /// Ground truth: hand the file to a real ZIP reader and ask it for the text
    /// back.  Skipped where `unzip` is not installed — every structural check
    /// above still runs, but only this one proves the archive opens.
    #[test]
    fn test_odt_opens_in_a_real_zip_reader() {
        if std::process::Command::new("unzip").arg("-v").output().is_err() {
            eprintln!("skipping: no `unzip` on this machine");
            return;
        }
        let dir = temp_dir("unzip");
        let mut job = SpoolJob::new();
        // CR LF, which is what CP/M software sends: a bare LF would leave the
        // carriage where it was and the indent below would start from there.
        for &b in b"THE QUICK BROWN FOX\r\n    INDENTED\x0CPAGE TWO" {
            job.push(b);
        }
        let name = job.write(dir.to_str().unwrap(), Format::Odt).expect("write");
        let path = dir.join(&name);

        let ok = std::process::Command::new("unzip")
            .arg("-t")
            .arg(&path)
            .output()
            .expect("unzip -t");
        assert!(ok.status.success(), "unzip rejected the archive: {}", String::from_utf8_lossy(&ok.stderr));

        let out = std::process::Command::new("unzip")
            .arg("-p")
            .arg(&path)
            .arg("content.xml")
            .output()
            .expect("unzip -p");
        let xml = String::from_utf8_lossy(&out.stdout);
        assert!(xml.contains("THE QUICK BROWN FOX"), "the text did not survive the round trip");
        assert!(xml.contains("<text:s text:c=\"4\"/>INDENTED"), "the indent was lost");
        assert!(xml.contains("PAGE TWO"));
        assert!(xml.contains("fo:break-before=\"page\""), "the form feed did not become a break");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
