use clap::Parser;
use nix::sys::ptrace::{self, Options};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{execvp, fork, ForkResult, Pid};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use termios::*;

/// rastreador de system calls tipo strace
#[derive(Parser, Debug)]
#[command(name = "rastreador", about = "Rastreador de system calls (tipo strace)")]
struct Args {
    /// modo verbose: imprime cada syscall con detalles
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// verbose con pausa: se detiene en cada syscall hasta que presiones una tecla
    #[arg(short = 'V')]
    verbose_pause: bool,

    /// programa a ejecutar (prog)
    #[arg(required = true)]
    prog: String,

    /// argumentos para prog (todo lo que sigue)
    #[arg(trailing_var_arg = true)]
    prog_args: Vec<String>,
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn syscall_num(regs: &libc::user_regs_struct) -> i64 {
    regs.orig_rax as i64
}
#[cfg(target_arch = "x86_64")]
#[inline]
fn syscall_ret(regs: &libc::user_regs_struct) -> i64 {
    regs.rax as i64
}

// en x86_64 los argumentos de syscall van en: rdi, rsi, rdx, r10, r8, r9
#[cfg(target_arch = "x86_64")]
fn syscall_args(regs: &libc::user_regs_struct) -> [u64; 6] {
    [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9]
}

// mapa parcial numero -> nombre; si no existe se muestra sys_<num>
fn syscall_name(n: i64) -> &'static str {
    match n {
        0 => "read",
        1 => "write",
        2 => "open",
        3 => "close",
        5 => "fstat",
        9 => "mmap",
        10 => "mprotect",
        12 => "brk",
        13 => "rt_sigaction",
        14 => "rt_sigprocmask",
        32 => "dup",
        33 => "dup2",
        56 => "clone",
        59 => "execve",
        60 => "exit",
        61 => "wait4",
        62 => "lseek",
        63 => "readv",
        64 => "writev",
        186 => "gettid",
        202 => "futex",
        218 => "set_tid_address",
        231 => "exit_group",
        257 => "openat",
        273 => "set_robust_list",
        302 => "prlimit64",
        318 => "getrandom",
        _ => "unknown",
    }
}

// lee una cadena terminada en 0 del proceso hijo usando process_vm_readv
// si falla, se retorna none y se imprime la direccion
fn read_string_from_child(pid: Pid, addr: u64, max_len: usize) -> Option<String> {
    if addr == 0 {
        return None;
    }
    unsafe {
        let mut buf = vec![0u8; max_len];
        let local_iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut _,
            iov_len: buf.len(),
        };
        let remote_iov = libc::iovec {
            iov_base: addr as *mut _,
            iov_len: buf.len(),
        };
        let nread = libc::process_vm_readv(
            pid.as_raw() as i32,
            &local_iov as *const libc::iovec,
            1,
            &remote_iov as *const libc::iovec,
            1,
            0,
        );
        if nread > 0 {
            let n = nread as usize;
            if let Some(pos) = buf[..n].iter().position(|&b| b == 0) {
                buf.truncate(pos);
            } else {
                buf.truncate(n);
            }
            if let Ok(s) = String::from_utf8(buf) {
                return Some(s);
            }
        }
    }
    None
}

// pone la terminal en modo sin canon para leer cualquier tecla
fn wait_keypress() {
    let fd = 0; // descriptor de stdin
    let mut term = Termios::from_fd(fd).expect("termios");
    let orig = term.clone();
    // desactiva modo canonico y eco
    term.c_lflag &= !(ICANON | ECHO);
    tcsetattr(fd, TCSANOW, &term).ok();
    let _ = io::stderr().write_all(b"[pausa] Presiona cualquier tecla...\n");
    let mut b = [0u8; 1];
    let _ = io::stdin().read_exact(&mut b);
    // restaura la terminal
    tcsetattr(fd, TCSANOW, &orig).ok();
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // arma argv para execvp
    let prog_c = CString::new(args.prog.clone())?;
    let mut argv: Vec<CString> = Vec::with_capacity(1 + args.prog_args.len());
    argv.push(prog_c.clone());
    for a in &args.prog_args {
        argv.push(CString::new(a.as_str())?);
    }

    match unsafe { fork()? } {
        ForkResult::Child => {
            // el hijo pide ser trazado
            ptrace::traceme().expect("ptrace TRACEME failed");
            // se envia sigstop para que el padre se conecte
            unsafe { libc::raise(libc::SIGSTOP) };
            // se ejecuta el programa objetivo
            execvp(&prog_c, &argv).expect("execvp failed");
        }
        ForkResult::Parent { child } => {
            // esperar a que el hijo se detenga con sigstop inicial
            waitpid(child, None).expect("waitpid (initial) failed");

            // configurar opciones de ptrace para marcar paradas de syscall
            ptrace::setoptions(
                child,
                Options::PTRACE_O_TRACESYSGOOD
                    | Options::PTRACE_O_TRACEEXEC
                    | Options::PTRACE_O_TRACEFORK
                    | Options::PTRACE_O_TRACECLONE
                    | Options::PTRACE_O_TRACEVFORK,
            )
            .expect("ptrace setoptions failed");

            // estructuras de conteo y estado de entrada/salida
            let mut counts: HashMap<i64, u64> = HashMap::new();
            let mut in_syscall = false;
            let mut current_sys: i64 = -1;

            // continuar hasta la primera parada de syscall
            ptrace::syscall(child, None).expect("ptrace syscall begin");

            loop {
                match waitpid(child, None).expect("waitpid failed") {
                    // parada de syscall
                    WaitStatus::PtraceSyscall(pid) => {
                        let regs = read_regs(pid);

                        if !in_syscall {
                            // entrada de syscall
                            current_sys = syscall_num(&regs);
                            *counts.entry(current_sys).or_default() += 1;

                            if args.verbose || args.verbose_pause {
                                let name = syscall_name(current_sys);
                                let a = syscall_args(&regs);
                                let mut extra = String::new();
                                // intento de leer una ruta para algunos syscalls
                                if name == "execve" || name == "openat" || name == "open" {
                                    if let Some(s) = read_string_from_child(pid, a[0], 4096) {
                                        extra = format!(" path=\"{}\"", s);
                                    }
                                }
                                eprintln!(
                                    "[syscall→] {} (#{}) args=[0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}]{}",
                                    name, current_sys, a[0], a[1], a[2], a[3], a[4], a[5], extra
                                );
                                if args.verbose_pause {
                                    wait_keypress();
                                }
                            }

                            in_syscall = true;
                        } else {
                            // salida de syscall
                            let ret = syscall_ret(&regs);
                            if args.verbose || args.verbose_pause {
                                let name = syscall_name(current_sys);
                                eprintln!("[←return] {} (#{}) = {}", name, current_sys, ret);
                                if args.verbose_pause {
                                    wait_keypress();
                                }
                            }
                            in_syscall = false;
                        }

                        // continuar hasta la proxima parada de syscall
                        ptrace::syscall(pid, None).expect("ptrace syscall continue");
                    }

                    // parada por senal normal: reenviar la senal y seguir
                    WaitStatus::Stopped(pid, sig) => {
                        ptrace::syscall(pid, Some(sig)).expect("ptrace syscall (forward signal)");
                    }

                    // el hijo termino normalmente
                    WaitStatus::Exited(_, status) => {
                        eprintln!("[hijo] terminado con exit({})", status);
                        break;
                    }

                    // el hijo termino por senal
                    WaitStatus::Signaled(_, sig, _core) => {
                        eprintln!("[hijo] terminado por señal {:?}", sig);
                        break;
                    }

                    // otros eventos de ptrace: solo continuar
                    WaitStatus::PtraceEvent(pid, _sig, _code) => {
                        ptrace::syscall(pid, None).expect("ptrace syscall continue (event)");
                    }

                    other => {
                        eprintln!("[info] evento no manejado: {:?}", other);
                    }
                }
            }

            // resumen final acumulado (ordenado por conteo descendente)
            let mut v: Vec<(i64, u64)> = counts.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

            println!("================= RESUMEN DE SYSTEM CALLS =================");
            println!("{:<24} {:<10} {}", "SYSCALL", "CUENTAS", "NUMERO");
            println!("-----------------------------------------------------------");
            for (num, cnt) in v {
                let name = syscall_name(num);
                if name == "unknown" {
                    println!("{:<24} {:<10} {}", format!("sys_{}", num), cnt, num);
                } else {
                    println!("{:<24} {:<10} {}", name, cnt, num);
                }
            }
            println!("===========================================================");
        }
    }

    Ok(())
}

fn read_regs(pid: Pid) -> libc::user_regs_struct {
    // obtiene los registros generales del hijo
    unsafe {
        let mut regs = MaybeUninit::<libc::user_regs_struct>::uninit();
        let ret = libc::ptrace(
            libc::PTRACE_GETREGS,
            pid.as_raw(),
            std::ptr::null_mut::<libc::c_void>(),
            regs.as_mut_ptr(),
        );
        if ret != 0 {
            panic!(
                "PTRACE_GETREGS failed: {}",
                std::io::Error::last_os_error()
            );
        }
        regs.assume_init()
    }
}

