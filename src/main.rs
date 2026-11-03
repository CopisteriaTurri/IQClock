#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! IQClock: alternativa minima a HP Click per DesignJet PostScript.

mod anteprima;
mod config;
#[cfg(target_os = "linux")]
mod desktop;
mod file;
mod gui;
mod impagina;
mod lavorazione;
mod pjl;
mod png;
mod rotazione;
mod snmp;
mod stampante;
mod stato;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use config::Config;
use impagina::{Allineamento, Impaginazione};
use lavorazione::{Lavorazione, Scala};
use pjl::{OpzioniJob, Sorgente};

const USO: &str = "\
Senza argomenti (o con dei file) apre l'interfaccia. Da riga di comando:
  iqclock stampa FILE... [--host IP] [--copie N] [--rotolo 1|2|auto] [--grigi] [--prn OUT]
                 [--allinea sinistra|centro|destra] [--larghezza-rotolo MM] [--margine MM]
                 [--scala A3|50%|500x700|500x700!]
      Con --allinea centro/destra la larghezza del rotolo si legge dalla stampante (SNMP);
      --larghezza-rotolo la indica a mano.
      --scala 500x700 tiene le proporzioni, 500x700! deforma.
  iqclock stato [--host IP]
  iqclock snmp OID [--host IP]      (diagnostica: elenca le voci sotto OID)";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("stampa") => riga_di_comando_stampa(&args[1..]),
        Some("stato") => riga_di_comando_stato(&args[1..]),
        Some("snmp") => riga_di_comando_snmp(&args[1..]),
        Some("-h" | "--help" | "aiuto") => {
            println!("{USO}");
            Ok(())
        }
        _ => avvia_gui(args.into_iter().map(PathBuf::from).collect()),
    }
}

fn avvia_gui(file: Vec<PathBuf>) -> Result<()> {
    let config = Config::carica();
    #[cfg(target_os = "linux")]
    if let Err(e) = desktop::installa(ICONA) {
        eprintln!("icona per il desktop non installata: {e}");
    }
    let mut finestra = eframe::egui::ViewportBuilder::default()
        .with_title("IQClock")
        .with_inner_size([1200.0, 760.0])
        .with_min_inner_size([800.0, 500.0])
        .with_drag_and_drop(true)
        // Su Wayland l'icona non passa dalla finestra: il desktop cerca il file
        // `iqclock.desktop` con lo stesso app_id (vedi `desktop.rs`).
        .with_app_id("iqclock");
    if let Some(icona) = icona_finestra() {
        finestra = finestra.with_icon(icona);
    }
    let opzioni = eframe::NativeOptions { viewport: finestra, ..Default::default() };
    eframe::run_native(
        "IQClock",
        opzioni,
        Box::new(move |cc| Ok(Box::new(gui::App::new(cc, config, file)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

const ICONA: &[u8] = include_bytes!("../assets/iqclock.png");

/// Icona della finestra e della taskbar: la stessa immagine che `build.rs`
/// incorpora nell'exe come `.ico`, così le due non divergono.
fn icona_finestra() -> Option<eframe::egui::IconData> {
    let img = image::load_from_memory(ICONA)
        .ok()?
        .to_rgba8();
    let (width, height) = img.dimensions();
    Some(eframe::egui::IconData { rgba: img.into_raw(), width, height })
}

/// Toglie `--nome valore` dagli argomenti e restituisce il valore.
fn prendi(args: &mut Vec<String>, nome: &str) -> Result<Option<String>> {
    let Some(i) = args.iter().position(|a| a == nome) else { return Ok(None) };
    if i + 1 >= args.len() {
        bail!("manca il valore di {nome}");
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Ok(Some(v))
}

fn prendi_flag(args: &mut Vec<String>, nome: &str) -> bool {
    let c = args.len();
    args.retain(|a| a != nome);
    args.len() != c
}

fn riga_di_comando_stampa(args: &[String]) -> Result<()> {
    let mut args = args.to_vec();
    let mut config = Config::carica();
    if let Some(h) = prendi(&mut args, "--host")? {
        config.host = h;
    }
    let copie = prendi(&mut args, "--copie")?.map(|c| c.parse()).transpose().context("--copie")?.unwrap_or(1);
    let sorgente = match prendi(&mut args, "--rotolo")?.as_deref() {
        None => Sorgente::Stampante,
        Some("1") => Sorgente::Rotolo1,
        Some("2") => Sorgente::Rotolo2,
        Some("auto") => Sorgente::Auto,
        Some(a) => bail!("--rotolo vuole 1, 2 o auto (non {a})"),
    };
    let scala_di_grigi = prendi_flag(&mut args, "--grigi");
    let prn = prendi(&mut args, "--prn")?.map(PathBuf::from);
    let allineamento = match prendi(&mut args, "--allinea")? {
        None => Allineamento::Sinistra,
        Some(a) => Allineamento::da_testo(&a)
            .with_context(|| format!("--allinea vuole sinistra, centro o destra (non {a})"))?,
    };
    let larghezza_rotolo: Option<f64> =
        prendi(&mut args, "--larghezza-rotolo")?.map(|m| m.parse()).transpose().context("--larghezza-rotolo")?;
    let margine_mm =
        prendi(&mut args, "--margine")?.map(|m| m.parse()).transpose().context("--margine")?.unwrap_or(config.margine_mm);
    let lavorazione = match prendi(&mut args, "--scala")? {
        None => Lavorazione::default(),
        Some(t) => Lavorazione {
            scala: Scala::da_testo(&t)
                .with_context(|| format!("--scala vuole A0..A4, 50% o 500x700 (non {t})"))?,
        },
    };
    if args.is_empty() {
        bail!("nessun file da stampare\n{USO}");
    }
    if prn.is_some() && args.len() > 1 {
        bail!("--prn accetta un solo file");
    }
    let impaginazione = if allineamento == Allineamento::Sinistra {
        None
    } else {
        // Impaginare su una larghezza sbagliata fa ridurre o tagliare la pagina:
        // o la dice la stampante, o la si indica esplicitamente.
        let rotolo_mm = match larghezza_rotolo {
            Some(mm) => mm,
            None => stato::leggi(&config.host)
                .context("lettura del rotolo dalla stampante")?
                .larghezza_rotolo_mm()
                .map(|(_, mm)| mm)
                .context("la stampante non dichiara la larghezza del rotolo: indicala con --larghezza-rotolo MM")?,
        };
        Some(Impaginazione { allineamento, rotolo_mm, margine_mm })
    };

    for f in args.iter().map(PathBuf::from) {
        let info = file::analizza(&f).with_context(|| f.display().to_string())?;
        if !lavorazione.nulla() {
            if !lavorazione::lavorabile(info.formato) {
                eprintln!("attenzione: {} è PostScript/HP-GL/2, la scala non si applica", f.display());
            } else if let Some((l, a)) = info.mm
                && lavorazione::fuori_limiti(&lavorazione.scala, l, a)
            {
                bail!(
                    "{}: la scala {} supera i limiti (da 1% a 1000% della misura originale)",
                    f.display(),
                    lavorazione.scala.etichetta()
                );
            }
        }
        let opz = OpzioniJob {
            nome: f.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            copie,
            sorgente,
            scala_di_grigi,
            impaginazione,
            lavorazione,
            ..Default::default()
        };
        match &prn {
            Some(out) => {
                let mut dest = std::fs::File::create(out)?;
                stampante::scrivi_job(&mut dest, &f, info.formato, &opz, |_| {})?;
                println!("{} → {}", f.display(), out.display());
            }
            None => {
                stampante::invia(&config.host, config.porta, &f, info.formato, &opz, |_| {})?;
                println!("{} → {}:{} ({})", f.display(), config.host, config.porta, info.dettagli);
            }
        }
    }
    Ok(())
}

fn riga_di_comando_stato(args: &[String]) -> Result<()> {
    let mut args = args.to_vec();
    let mut config = Config::carica();
    if let Some(h) = prendi(&mut args, "--host")? {
        config.host = h;
    }
    let s = stato::leggi(&config.host)?;
    println!("Modello:  {}", s.modello.as_deref().unwrap_or("?"));
    println!("Stato:    {}", s.descrizione());
    for c in &s.cartucce {
        let p = c.percentuale.map(|p| format!("{p:>3}%")).unwrap_or_else(|| "  ?".into());
        println!("Inchiostro {p}  {} {}", c.colore.unwrap_or(""), c.nome);
    }
    for i in &s.ingressi {
        println!("Carta:    {} — {}", i.nome, i.descrizione_lunga());
    }
    match s.larghezza_rotolo_mm() {
        Some((nome, mm)) => println!("Rotolo:   {mm:.0} mm (rilevato da {nome})"),
        None => println!("Rotolo:   non rilevato (MIB senza dimensioni o SNMP non raggiungibile)"),
    }
    Ok(())
}

fn riga_di_comando_snmp(args: &[String]) -> Result<()> {
    let mut args = args.to_vec();
    let mut config = Config::carica();
    if let Some(h) = prendi(&mut args, "--host")? {
        config.host = h;
    }
    let base = snmp::oid(args.first().map(String::as_str).unwrap_or("1.3.6.1.2.1.43"));
    for (o, v) in snmp::Client::new(&config.host, "public")?.walk(&base)? {
        let o: Vec<String> = o.iter().map(u32::to_string).collect();
        let v = v.testo().map(|t| format!("\"{t}\"")).unwrap_or_else(|| format!("{v:?}"));
        println!("{} = {v}", o.join("."));
    }
    Ok(())
}
