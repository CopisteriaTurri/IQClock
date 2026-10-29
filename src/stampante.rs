//! Comunicazione con la stampante via socket raw (porta 9100, "JetDirect").

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::pjl::{self, Formato, OpzioniJob};
use crate::png;

const TIMEOUT_CONNESSIONE: Duration = Duration::from_secs(5);
/// La stampante può rallentare la ricezione mentre elabora: timeout generoso.
const TIMEOUT_SCRITTURA: Duration = Duration::from_secs(120);

fn connetti(host: &str, porta: u16) -> Result<TcpStream> {
    let indirizzo = (host, porta)
        .to_socket_addrs()
        .with_context(|| format!("indirizzo non valido: {host}"))?
        .next()
        .with_context(|| format!("nessun indirizzo per {host}"))?;
    TcpStream::connect_timeout(&indirizzo, TIMEOUT_CONNESSIONE)
        .with_context(|| format!("stampante non raggiungibile su {indirizzo}"))
}

/// Scrive il job completo (intestazione PJL + file + chiusura) su `dest`,
/// chiamando `avanzamento` con la frazione già scritta (0.0..=1.0).
///
/// I PNG non vengono mandati così come sono: la stampante non li legge, quindi
/// si convertono in TIFF (vedi `png.rs`) e si invia quello.
pub fn scrivi_job(
    dest: &mut impl Write,
    percorso: &Path,
    formato: Formato,
    opz: &OpzioniJob,
    mut avanzamento: impl FnMut(f32),
) -> Result<()> {
    if formato == Formato::Png {
        let tiff = png::png_a_tiff(percorso)?;
        return scrivi_job(dest, tiff.percorso(), Formato::Tiff, opz, avanzamento);
    }

    let totale = std::fs::metadata(percorso)?.len().max(1);
    let mut file = BufReader::new(File::open(percorso)?);
    dest.write_all(&pjl::intestazione(opz, formato))?;

    let mut buf = vec![0u8; 256 * 1024];
    let mut scritti = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dest.write_all(&buf[..n])?;
        scritti += n as u64;
        avanzamento(scritti as f32 / totale as f32);
    }

    dest.write_all(&pjl::chiusura(opz))?;
    dest.flush()?;
    Ok(())
}

pub fn invia(
    host: &str,
    porta: u16,
    percorso: &Path,
    formato: Formato,
    opz: &OpzioniJob,
    avanzamento: impl FnMut(f32),
) -> Result<()> {
    // Il PNG si converte prima di connettersi, così la stampante non resta col
    // socket aperto e senza dati per tutta la conversione.
    if formato == Formato::Png {
        let tiff = png::png_a_tiff(percorso)?;
        return invia(host, porta, tiff.percorso(), Formato::Tiff, opz, avanzamento);
    }
    let mut sock = connetti(host, porta)?;
    sock.set_write_timeout(Some(TIMEOUT_SCRITTURA))?;
    scrivi_job(&mut sock, percorso, formato, opz, avanzamento).context("invio interrotto")?;
    // Chiude il lato di scrittura: per la stampante il job è finito.
    let _ = sock.shutdown(Shutdown::Write);
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// File temporaneo cancellato anche se il test fallisce a metà.
    struct FileTemp(std::path::PathBuf);

    impl FileTemp {
        fn nuovo(nome: &str, contenuto: &[u8]) -> Self {
            let p = std::env::temp_dir().join(format!("iqclock-{nome}-{}.pdf", std::process::id()));
            std::fs::write(&p, contenuto).unwrap();
            Self(p)
        }
    }

    impl Drop for FileTemp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn i_png_partono_come_tiff() {
        let p = std::env::temp_dir().join(format!("iqclock-png-{}.png", std::process::id()));
        image::save_buffer(&p, &[10, 20, 30, 40, 50, 60], 2, 1, image::ColorType::Rgb8).unwrap();
        let opz = OpzioniJob { nome: "logo".into(), ..Default::default() };
        let mut job = Vec::new();
        scrivi_job(&mut job, &p, Formato::Png, &opz, |_| {}).unwrap();
        std::fs::remove_file(&p).unwrap();

        let testa = String::from_utf8_lossy(&job[..job.len().min(256)]).replace('\x1b', "<ESC>");
        assert!(testa.contains("@PJL JOB NAME=\"logo\""), "{testa}");
        assert!(!testa.contains("ENTER LANGUAGE"), "i TIFF non hanno ENTER LANGUAGE: {testa}");
        // Dopo l'intestazione arrivano i byte del TIFF (little endian).
        assert!(job.windows(4).any(|w| w == b"II*\0"), "manca la firma TIFF nel job");
        assert!(job.ends_with(pjl::UEL));
        let testo = String::from_utf8_lossy(&job[job.len() - 120..]);
        assert!(testo.contains("@PJL EOJ NAME=\"logo\""));
    }

    #[test]
    fn file_grande_arriva_integro() {
        // Più del buffer interno da 256 KiB: verifica streaming a chunk.
        let contenuto: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let p = FileTemp::nuovo("grande", &contenuto);
        let opz = OpzioniJob { nome: "grande".into(), ..Default::default() };
        let mut passi = Vec::new();
        let mut job = Vec::new();
        scrivi_job(&mut job, &p.0, Formato::Pdf, &opz, |f| passi.push(f)).unwrap();

        let testa = pjl::intestazione(&opz, Formato::Pdf);
        let coda = pjl::chiusura(&opz);
        assert!(job.starts_with(&testa));
        assert!(job.ends_with(&coda));
        assert_eq!(&job[testa.len()..job.len() - coda.len()], &contenuto);
        assert!(passi.len() > 2, "l'avanzamento deve scattare più volte");
        assert!(passi.windows(2).all(|w| w[0] <= w[1]), "avanzamento monotono");
        assert_eq!(passi.last(), Some(&1.0));
    }

    #[test]
    fn connessione_chiusa_a_meta_da_errore_leggibile() {
        // Finta stampante che legge solo l'intestazione e chiude di colpo:
        // l'invio deve fallire, non risultare riuscito.
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let porta = server.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            // Legge solo l'intestazione, poi chiude di colpo senza leggere il
            // resto: chiudere con dati non letti in coda manda un RST.
            let mut testa = [0u8; 512];
            let _ = s.read_exact(&mut testa);
            // `s` droppato qui.
        });

        let p = FileTemp::nuovo("rst", &vec![0xABu8; 16 * 1024 * 1024]);
        let opz = OpzioniJob { nome: "rst".into(), ..Default::default() };
        let err = invia("127.0.0.1", porta, &p.0, Formato::Pdf, &opz, |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("invio interrotto"), "{err:#}");
    }

    #[test]
    fn porta_chiusa_da_errore_leggibile() {
        // Porta libera ma senza nessuno in ascolto: rifiuto immediato.
        let porta = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let p = FileTemp::nuovo("rifiuto", b"%PDF-1.4 finto");
        let opz = OpzioniJob::default();
        let err = invia("127.0.0.1", porta, &p.0, Formato::Pdf, &opz, |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("stampante non raggiungibile"), "{err:#}");
    }

    #[test]
    fn host_invalido_da_errore_leggibile() {
        // .invalid è riservato (RFC 6761): non si risolve su nessuna rete.
        let p = FileTemp::nuovo("host", b"%PDF-1.4 finto");
        let opz = OpzioniJob::default();
        let err = invia("stampante.invalid", 9100, &p.0, Formato::Pdf, &opz, |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("indirizzo non valido"), "{err:#}");
    }

    #[test]
    fn invio_a_finta_stampante() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let porta = server.local_addr().unwrap().port();
        let ricevuto = thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            let mut v = Vec::new();
            s.read_to_end(&mut v).unwrap();
            v
        });

        let p = std::env::temp_dir().join(format!("iqclock-invio-{}.pdf", std::process::id()));
        std::fs::write(&p, b"%PDF-1.4 finto").unwrap();
        let opz = OpzioniJob { nome: "prova".into(), ..Default::default() };
        let mut ultimo = 0.0;
        invia("127.0.0.1", porta, &p, Formato::Pdf, &opz, |f| ultimo = f).unwrap();
        std::fs::remove_file(&p).unwrap();

        let v = String::from_utf8(ricevuto.join().unwrap()).unwrap();
        assert!(v.starts_with("\x1b%-12345X@PJL JOB NAME=\"prova\""));
        assert!(v.contains("@PJL ENTER LANGUAGE=PDF\r\n%PDF-1.4 finto\x1b%-12345X@PJL EOJ"));
        assert!(v.ends_with("\x1b%-12345X"));
        assert_eq!(ultimo, 1.0);
    }
}
