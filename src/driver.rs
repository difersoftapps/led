//! Protocolo da MCU de LED + thread de envio.
//!
//! O envio de um comando completo leva centenas de milissegundos (cada byte vai
//! 6x com 10 ms de intervalo). Para a interface ficar fluida, os comandos sao
//! executados numa thread propria e, se o usuario mudar de cor varias vezes
//! enquanto um envio esta em andamento, so o ULTIMO estado pedido e enviado em
//! seguida (os intermediarios sao descartados).

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd, io::RawFd},
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const DEFAULT_DEV: &str = "/dev/ttyS1";
pub const DEFAULT_PWR: &str = "/sys/kernel/debug/regulator/vcc-led/enable";
/// `chgmode 9` = apagar.
pub const MODE_OFF: u8 = 9;
pub const BRI_MIN: u8 = 1;
pub const BRI_MAX: u8 = 9;

/// Byte de handshake `init <ledcount>`: `0x20 + ledcount/4 - 1` (12 LEDs -> 0x22).
/// Inferido pelo autor do ledctl.sh a partir de 4 -> 0x20 e 8 -> 0x21.
pub fn init_byte(ledcount: u32) -> Option<u8> {
    if ledcount < 4 {
        return None;
    }
    u8::try_from(0x20 + ledcount / 4 - 1).ok()
}

/// Brilho 1 (fraco) .. 9 (forte) -> byte `50 - B`.
pub fn bri_byte(bri: u8) -> u8 {
    50 - bri.clamp(BRI_MIN, BRI_MAX)
}

// ---------------------------------------------------------------------------
// Transporte
// ---------------------------------------------------------------------------

pub trait Transport: Send {
    /// Liga/desliga o trilho de alimentacao dos LEDs (regulador `vcc-led`).
    fn power(&mut self, on: bool) -> io::Result<()>;
    /// Abre a UART (somente escrita, 9600 8N1 raw).
    fn open(&mut self) -> io::Result<()>;
    fn send_byte(&mut self, byte: u8) -> io::Result<()>;
    fn close(&mut self);
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn power(&mut self, on: bool) -> io::Result<()> {
        (**self).power(on)
    }
    fn open(&mut self) -> io::Result<()> {
        (**self).open()
    }
    fn send_byte(&mut self, byte: u8) -> io::Result<()> {
        (**self).send_byte(byte)
    }
    fn close(&mut self) {
        (**self).close()
    }
}

/// Transporte real: `/dev/ttyS1` + debugfs do regulador. Requer root.
pub struct UartTransport {
    dev: PathBuf,
    pwr: PathBuf,
    file: Option<File>,
}

impl UartTransport {
    pub fn new(dev: impl Into<PathBuf>, pwr: impl Into<PathBuf>) -> Self {
        UartTransport {
            dev: dev.into(),
            pwr: pwr.into(),
            file: None,
        }
    }
}

fn last_err() -> io::Error {
    io::Error::last_os_error()
}

/// Equivalente a: stty 9600 cs8 -cstopb -parenb clocal -crtscts -ixon raw -echo
fn configure_raw_9600(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` e um descritor valido aberto por nos; `t` e inicializado por tcgetattr.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return Err(last_err());
        }
        libc::cfmakeraw(&mut t);
        t.c_cflag |= libc::CLOCAL | libc::CS8;
        t.c_cflag &= !(libc::CSTOPB | libc::PARENB | libc::CRTSCTS);
        t.c_iflag &= !(libc::IXON | libc::IXOFF);
        if libc::cfsetispeed(&mut t, libc::B9600) != 0
            || libc::cfsetospeed(&mut t, libc::B9600) != 0
        {
            return Err(last_err());
        }
        if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
            return Err(last_err());
        }
    }
    Ok(())
}

fn clear_nonblock(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl em descritor valido.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl & !libc::O_NONBLOCK) < 0 {
            return Err(last_err());
        }
    }
    Ok(())
}

impl Transport for UartTransport {
    fn power(&mut self, on: bool) -> io::Result<()> {
        fs::write(&self.pwr, if on { "1" } else { "0" })
    }

    fn open(&mut self) -> io::Result<()> {
        // O_NONBLOCK so durante a configuracao: evita travar esperando portadora.
        let f = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(&self.dev)?;
        let fd = f.as_raw_fd();
        configure_raw_9600(fd)?;
        clear_nonblock(fd)?;
        self.file = Some(f);
        Ok(())
    }

    fn send_byte(&mut self, byte: u8) -> io::Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "UART fechada"))?
            .write_all(&[byte])
    }

    fn close(&mut self) {
        self.file = None;
    }
}

/// Eventos gravados/mostrados pelo transporte de teste.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Power(bool),
    Open,
    Byte(u8),
    Close,
}

/// Transporte que nao toca no hardware: so registra (e opcionalmente imprime) o
/// que seria enviado. Usado por `--dry-run` e pelos testes.
pub struct DryRun {
    log: Arc<Mutex<Vec<Event>>>,
    verbose: bool,
}

impl DryRun {
    /// Imprime cada evento em stderr.
    pub fn verbose() -> Self {
        DryRun {
            log: Arc::default(),
            verbose: true,
        }
    }

    /// Silencioso; devolve tambem o registro compartilhado.
    pub fn recorder() -> (Self, Arc<Mutex<Vec<Event>>>) {
        let log: Arc<Mutex<Vec<Event>>> = Arc::default();
        (
            DryRun {
                log: log.clone(),
                verbose: false,
            },
            log,
        )
    }

    fn push(&self, e: Event) {
        if self.verbose {
            eprintln!("[dry-run] {e:?}");
        }
        self.log.lock().unwrap().push(e);
    }
}

impl Transport for DryRun {
    fn power(&mut self, on: bool) -> io::Result<()> {
        self.push(Event::Power(on));
        Ok(())
    }
    fn open(&mut self) -> io::Result<()> {
        self.push(Event::Open);
        Ok(())
    }
    fn send_byte(&mut self, byte: u8) -> io::Result<()> {
        self.push(Event::Byte(byte));
        Ok(())
    }
    fn close(&mut self) {
        self.push(Event::Close);
    }
}

// ---------------------------------------------------------------------------
// Sequencia do protocolo
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Intervalo entre repeticoes do mesmo byte (script: 10 ms).
    pub gap: Duration,
    /// Pausa depois do byte de init (script: 100 ms).
    pub after_init: Duration,
    /// Pausa depois do byte de modo (script: 50 ms).
    pub after_mode: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            gap: Duration::from_millis(10),
            after_init: Duration::from_millis(100),
            after_mode: Duration::from_millis(50),
        }
    }
}

impl Timing {
    pub fn none() -> Self {
        Timing {
            gap: Duration::ZERO,
            after_init: Duration::ZERO,
            after_mode: Duration::ZERO,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Quantas vezes cada byte e enviado (script: 6).
    pub repeat: usize,
    /// `false` (padrao): o init vai so apos ligar o trilho. `true`: em todo envio,
    /// exatamente como o ledctl.sh faz (use se a MCU parecer ignorar trocas).
    pub always_init: bool,
    pub ledcount: u32,
    pub timing: Timing,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            repeat: 6,
            always_init: false,
            ledcount: 12,
            timing: Timing::default(),
        }
    }
}

/// Estado desejado dos LEDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Desired {
    pub enabled: bool,
    pub mode: u8,
    pub bri: u8,
}

pub struct Controller<T: Transport> {
    t: T,
    opt: Options,
    powered: bool,
    initialized: bool,
    power_warned: bool,
}

impl<T: Transport> Controller<T> {
    pub fn new(t: T, opt: Options) -> Self {
        Controller {
            t,
            opt,
            powered: false,
            initialized: false,
            power_warned: false,
        }
    }

    pub fn apply(&mut self, d: Desired) -> io::Result<()> {
        if !d.enabled {
            return self.off();
        }
        if !self.powered {
            self.set_power(true);
            self.powered = true;
            // MCU recem-energizada: precisa do handshake de novo.
            self.initialized = false;
        }
        self.t.open()?;
        let r = self.on_sequence(d);
        self.t.close();
        r
    }

    fn on_sequence(&mut self, d: Desired) -> io::Result<()> {
        if !self.initialized || self.opt.always_init {
            if let Some(ib) = init_byte(self.opt.ledcount) {
                self.send(ib)?;
                thread::sleep(self.opt.timing.after_init);
            }
            self.initialized = true;
        }
        self.send(d.mode)?;
        thread::sleep(self.opt.timing.after_mode);
        self.send(bri_byte(d.bri))
    }

    /// `light_off` do script: chgmode 9, depois derruba o trilho.
    fn off(&mut self) -> io::Result<()> {
        let r = match self.t.open() {
            Ok(()) => {
                let r = self.send(MODE_OFF);
                self.t.close();
                r
            }
            Err(e) => Err(e),
        };
        self.set_power(false);
        self.powered = false;
        self.initialized = false;
        r
    }

    fn send(&mut self, byte: u8) -> io::Result<()> {
        for _ in 0..self.opt.repeat.max(1) {
            self.t.send_byte(byte)?;
            thread::sleep(self.opt.timing.gap);
        }
        Ok(())
    }

    /// Falha no trilho nao e fatal (o script tambem ignora): so avisa uma vez.
    fn set_power(&mut self, on: bool) {
        if let Err(e) = self.t.power(on) {
            if !self.power_warned {
                self.power_warned = true;
                eprintln!("aviso: nao consegui controlar o trilho vcc-led ({e}); precisa de root e debugfs montado");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Thread de envio com "coalescencia" de comandos
// ---------------------------------------------------------------------------

enum Msg {
    Set(Desired),
    Quit,
}

#[derive(Default)]
struct Shared {
    pending: AtomicUsize,
    applied: AtomicU32,
    error: Mutex<Option<String>>,
}

pub struct LedHandle {
    tx: mpsc::Sender<Msg>,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl LedHandle {
    pub fn spawn<T: Transport + 'static>(ctrl: Controller<T>) -> LedHandle {
        let (tx, rx) = mpsc::channel();
        let shared: Arc<Shared> = Arc::default();
        let sh = shared.clone();
        let join = thread::Builder::new()
            .name("led-tx".into())
            .spawn(move || run(ctrl, rx, sh))
            .expect("nao foi possivel criar a thread de LED");
        LedHandle {
            tx,
            shared,
            join: Some(join),
        }
    }

    /// Pede o estado `d`. Nao bloqueia.
    pub fn set(&self, d: Desired) {
        self.shared.pending.fetch_add(1, Ordering::AcqRel);
        if self.tx.send(Msg::Set(d)).is_err() {
            self.shared.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Ha comandos na fila ou um envio em andamento.
    pub fn busy(&self) -> bool {
        self.shared.pending.load(Ordering::Acquire) > 0
    }

    /// Ultimo erro de envio (limpo quando um envio posterior da certo).
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    /// Quantos envios completos ja foram feitos (apos coalescer).
    pub fn applied(&self) -> u32 {
        self.shared.applied.load(Ordering::Acquire)
    }

    /// Espera a fila esvaziar. `false` se estourou o tempo.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while self.busy() {
            if start.elapsed() > timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
        true
    }

    /// Termina a thread depois de enviar o que ainda estiver na fila.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.tx.send(Msg::Quit);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for LedHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run<T: Transport>(mut ctrl: Controller<T>, rx: mpsc::Receiver<Msg>, sh: Arc<Shared>) {
    let mut last_ok: Option<Desired> = None;
    loop {
        let Ok(first) = rx.recv() else { return };
        let mut latest: Option<Desired> = None;
        let mut sets = 0usize;
        let mut quit = false;
        for m in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            match m {
                Msg::Set(d) => {
                    latest = Some(d);
                    sets += 1;
                }
                Msg::Quit => quit = true,
            }
        }
        if let Some(d) = latest {
            if last_ok != Some(d) {
                match ctrl.apply(d) {
                    Ok(()) => {
                        last_ok = Some(d);
                        *sh.error.lock().unwrap() = None;
                        sh.applied.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(e) => {
                        last_ok = None;
                        *sh.error.lock().unwrap() = Some(e.to_string());
                    }
                }
            }
        }
        if sets > 0 {
            sh.pending.fetch_sub(sets, Ordering::AcqRel);
        }
        if quit {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Event::*;

    fn on(mode: u8, bri: u8) -> Desired {
        Desired {
            enabled: true,
            mode,
            bri,
        }
    }

    fn ctrl(always_init: bool, timing: Timing) -> (Controller<DryRun>, Arc<Mutex<Vec<Event>>>) {
        let (t, log) = DryRun::recorder();
        let opt = Options {
            repeat: 2,
            always_init,
            ledcount: 12,
            timing,
        };
        (Controller::new(t, opt), log)
    }

    fn take(log: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    #[test]
    fn bytes_do_protocolo() {
        assert_eq!(init_byte(4), Some(0x20));
        assert_eq!(init_byte(8), Some(0x21));
        assert_eq!(init_byte(12), Some(0x22));
        assert_eq!(init_byte(3), None);
        assert_eq!(bri_byte(1), 49);
        assert_eq!(bri_byte(5), 45);
        assert_eq!(bri_byte(9), 41);
        assert_eq!(bri_byte(0), 49); // limitado
        assert_eq!(bri_byte(200), 41);
    }

    #[test]
    fn ligar_manda_init_modo_brilho() {
        let (mut c, log) = ctrl(false, Timing::none());
        c.apply(on(3, 5)).unwrap();
        assert_eq!(
            take(&log),
            vec![
                Power(true),
                Open,
                Byte(0x22),
                Byte(0x22),
                Byte(3),
                Byte(3),
                Byte(45),
                Byte(45),
                Close
            ]
        );
    }

    #[test]
    fn troca_seguinte_nao_repete_init_nem_trilho() {
        let (mut c, log) = ctrl(false, Timing::none());
        c.apply(on(3, 5)).unwrap();
        take(&log);
        c.apply(on(19, 7)).unwrap();
        assert_eq!(
            take(&log),
            vec![Open, Byte(19), Byte(19), Byte(43), Byte(43), Close]
        );
    }

    #[test]
    fn always_init_repete_o_handshake() {
        let (mut c, log) = ctrl(true, Timing::none());
        c.apply(on(3, 5)).unwrap();
        take(&log);
        c.apply(on(1, 5)).unwrap();
        let ev = take(&log);
        assert_eq!(ev[1], Byte(0x22));
        assert_eq!(ev.len(), 8); // Open, init x2, modo x2, bri x2, Close
    }

    #[test]
    fn desligar_manda_9_e_corta_o_trilho_e_religar_refaz_init() {
        let (mut c, log) = ctrl(false, Timing::none());
        c.apply(on(3, 5)).unwrap();
        take(&log);
        c.apply(Desired {
            enabled: false,
            mode: 3,
            bri: 5,
        })
        .unwrap();
        assert_eq!(
            take(&log),
            vec![Open, Byte(9), Byte(9), Close, Power(false)]
        );
        c.apply(on(3, 5)).unwrap();
        let ev = take(&log);
        assert_eq!(ev[0], Power(true));
        assert!(ev.contains(&Byte(0x22)), "init deve voltar apos religar");
    }

    #[test]
    fn rajada_de_comandos_e_coalescida_no_ultimo() {
        let (mut c, log) = ctrl(
            false,
            Timing {
                gap: Duration::ZERO,
                after_init: Duration::ZERO,
                after_mode: Duration::from_millis(40),
            },
        );
        c.opt.repeat = 1;
        let h = LedHandle::spawn(c);
        for m in 1..=8u8 {
            h.set(on(m, 5));
        }
        assert!(h.wait_idle(Duration::from_secs(3)));
        assert!(h.error().is_none());
        let n = h.applied();
        assert!((1..=3).contains(&n), "esperava poucos envios, foram {n}");
        h.shutdown();
        // O ultimo modo pedido (8) tem que ter sido o ultimo a chegar na MCU.
        let modes: Vec<u8> = take(&log)
            .into_iter()
            .filter_map(|e| match e {
                Byte(b) if b != 0x22 && b != 45 => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(modes.last(), Some(&8), "{modes:?}");
        assert!(modes.len() <= 3, "{modes:?}");
    }

    #[test]
    fn erro_de_transporte_aparece_no_handle() {
        struct Quebrado;
        impl Transport for Quebrado {
            fn power(&mut self, _: bool) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
            fn open(&mut self) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::NotFound, "sem /dev/ttyS1"))
            }
            fn send_byte(&mut self, _: u8) -> io::Result<()> {
                unreachable!()
            }
            fn close(&mut self) {}
        }
        let h = LedHandle::spawn(Controller::new(Quebrado, Options::default()));
        h.set(on(3, 5));
        assert!(h.wait_idle(Duration::from_secs(2)));
        assert!(h.error().unwrap().contains("ttyS1"));
        assert_eq!(h.applied(), 0);
    }
}
