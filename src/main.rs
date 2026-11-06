#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! IQClock: alternativa minima a HP Click per DesignJet PostScript.

mod anteprima;
mod config;
#[cfg(target_os = "linux")]
mod desktop;
mod file;
mod foglio;
mod gui;
mod impagina;
mod lavorazione;
mod nesting;
mod pjl;
mod png;
mod risoluzione;
mod rotazione;
mod snmp;
mod stampante;
mod stato;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use config::Config;
use impagina::{Allineamento, Impaginazione};
use lavorazione::{Cornice, DaAnnidare, Lavorazione, Scala};
use nesting::Nesting;
use pjl::{Formato, OpzioniJob, Sorgente};

const USO: &str = "\
Senza argomenti (o con dei file) apre l'interfaccia. Da riga di comando:
  iqclock stampa FILE... [--host IP] [--copie N] [--rotolo 1|2|auto|foglio] [--grigi] [--prn OUT]
                 [--allinea sinistra|centro|destra] [--larghezza-rotolo MM] [--margine MM]
                 [--scala A3|50%|500x700|500x700!] [--cornice]
                 [--nesting] [--distanza MM] [--senza-rotazione]
      Con --allinea centro/destra, o con --nesting, la larghezza del rotolo si legge dalla
      stampante (SNMP); --larghezza-rotolo la indica a mano.
      --scala 500x700 tiene le proporzioni, 500x700! deforma.
      --cornice aggiunge la linea di taglio (distanza e spessore dalla configurazione).
      --nesting mette più file sullo stesso foglio (PDF e immagini): un job per foglio,
      distanza tra i pezzi in mm e ruota i pezzi per stare nel rotolo (--senza-rotazione
      li lascia come sono). Con --prn scrive solo il primo foglio.
      --rotolo foglio stampa su foglio singolo dall'ingresso manuale: il file parte
      della sua misura, senza allineamento né nesting.
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

/// Avvia la finestra con le dimensioni configurate e i file iniziali;
/// propaga gli errori di avvio dell’interfaccia.
fn avvia_gui(file: Vec<PathBuf>) -> Result<()> {
    let config = Config::carica();
    #[cfg(target_os = "linux")]
    if let Err(e) = desktop::installa(ICONA) {
        eprintln!("icona per il desktop non installata: {e}");
    }
    let mut finestra = eframe::egui::ViewportBuilder::default()
        .with_title("IQClock")
        // Alta abbastanza da mostrare tutte le sezioni del pannello; da
        // configurazione si può cambiare.
        .with_inner_size([
            config.finestra_larghezza_px as f32,
            config.finestra_altezza_px as f32,
        ])
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

/// Legge opzioni e file della CLI, applica scala, cornice e allineamento,
/// poi invia i job alla stampante o li salva con --prn.
/// Restituisce un errore per argomenti non validi o preparazione/invio falliti.
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
        Some("foglio") => Sorgente::Foglio,
        Some(a) => bail!("--rotolo vuole 1, 2, auto o foglio (non {a})"),
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
    let scala = match prendi(&mut args, "--scala")? {
        None => Scala::Originale,
        Some(t) => Scala::da_testo(&t)
            .with_context(|| format!("--scala vuole A0..A4, 50% o 500x700 (non {t})"))?,
    };
    let cornice = prendi_flag(&mut args, "--cornice").then_some(Cornice {
        distanza_mm: config.cornice_distanza_mm,
        spessore_mm: config.cornice_spessore_mm,
    });
    let lavorazione = Lavorazione { scala, cornice, ..Default::default() };
    let nesting = prendi_flag(&mut args, "--nesting");
    let distanza_mm = prendi(&mut args, "--distanza")?
        .map(|m| m.parse::<f64>())
        .transpose()
        .context("--distanza")?
        .unwrap_or(config.nesting_distanza_mm);
    if !(distanza_mm.is_finite() && distanza_mm >= 0.0) {
        bail!("--distanza vuole un numero di mm maggiore o uguale a 0");
    }
    let ruota = !prendi_flag(&mut args, "--senza-rotazione");
    // Sul foglio singolo non c'è un rotolo da riempire: nesting e allineamento
    // non si applicano e la larghezza del rotolo non serve.
    let foglio = sorgente == Sorgente::Foglio;
    if foglio {
        if nesting {
            eprintln!("attenzione: con --rotolo foglio il nesting non si applica");
        }
        if allineamento != Allineamento::Sinistra {
            eprintln!("attenzione: con --rotolo foglio l'allineamento non si applica");
        }
    }
    let nesting = nesting && !foglio;
    if args.is_empty() {
        bail!("nessun file da stampare\n{USO}");
    }
    if prn.is_some() && args.len() > 1 && !nesting {
        bail!("--prn accetta un solo file (con --nesting scrive il primo foglio)");
    }
    // Impaginare (o annidare) su una larghezza sbagliata fa ridurre o tagliare
    // la pagina: o la dice la stampante, o la si indica esplicitamente.
    let rotolo_mm = match larghezza_rotolo {
        Some(mm) => Some(mm),
        None if !foglio && (nesting || allineamento != Allineamento::Sinistra) => Some(
            stato::leggi(&config.host)
                .context("lettura del rotolo dalla stampante")?
                .larghezza_rotolo_mm()
                .map(|(_, mm)| mm)
                .context("la stampante non dichiara la larghezza del rotolo: indicala con --larghezza-rotolo MM")?,
        ),
        None => None,
    };
    // Con il nesting l'allineamento non si applica: il foglio è composto dai pezzi.
    let impaginazione = match (nesting, allineamento) {
        // Il foglio è già tagliato: una pagina, nessun rotolo da allineare.
        _ if foglio => None,
        (true, Allineamento::Sinistra) => None,
        (true, _) => {
            eprintln!("attenzione: con --nesting l'allineamento non si applica");
            None
        }
        (false, Allineamento::Sinistra) => None,
        (false, allineamento) => {
            Some(Impaginazione { allineamento, rotolo_mm: rotolo_mm.unwrap(), margine_mm })
        }
    };

    if nesting {
        return stampa_annidata(
            &config,
            args,
            prn,
            rotolo_mm.unwrap(),
            margine_mm,
            copie,
            sorgente,
            scala_di_grigi,
            lavorazione,
            Nesting { distanza_mm, ruota },
        );
    }

    for f in args.iter().map(PathBuf::from) {
        let info = file::analizza(&f).with_context(|| f.display().to_string())?;
        valida_scala(&f, &info, &lavorazione)?;
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

/// Applica gli stessi limiti di scala ai documenti singoli e annidati.
fn valida_scala(
    percorso: &std::path::Path,
    info: &file::InfoFile,
    lavorazione: &Lavorazione,
) -> Result<()> {
    if !lavorazione.nulla() {
        if !lavorazione::lavorabile(info.formato) {
            eprintln!(
                "attenzione: {} è PostScript/HP-GL/2, scala e cornice non si applicano",
                percorso.display()
            );
        } else if info.pagine_mm.iter().flatten().copied().chain(info.mm).any(|(l, a)| {
            lavorazione::fuori_limiti(&lavorazione.scala, l, a)
        }) {
            bail!(
                "{}: la scala {} supera i limiti (da 1% a 1000% della misura originale)",
                percorso.display(),
                lavorazione.scala.etichetta()
            );
        }
    }
    Ok(())
}

/// Percorso con `--nesting`: compone i fogli e manda un job per foglio, poi i
/// documenti esclusi uno alla volta come oggi. `--prn` scrive solo il primo
/// foglio, senza toccare la stampante.
#[allow(clippy::too_many_arguments)]
fn stampa_annidata(
    config: &Config,
    file: Vec<String>,
    prn: Option<PathBuf>,
    rotolo_mm: f64,
    margine_mm: f64,
    copie: u32,
    sorgente: Sorgente,
    scala_di_grigi: bool,
    lavorazione: Lavorazione,
    nesting: Nesting,
) -> Result<()> {
    let documenti: Vec<DaAnnidare> = file
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let percorso = PathBuf::from(f);
            let info = file::analizza(&percorso).with_context(|| percorso.display().to_string())?;
            valida_scala(&percorso, &info, &lavorazione)?;
            Ok(DaAnnidare {
                id: i as u64,
                percorso,
                formato: info.formato,
                lavorazione,
                copie,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let annidamento = lavorazione::componi_nesting(&documenti, rotolo_mm, margine_mm, &nesting)
        .context("composizione dei fogli")?;
    let base = OpzioniJob {
        nome: format!("Nesting ({} file)", annidamento.documenti),
        copie: 1,
        sorgente,
        scala_di_grigi,
        impaginazione: None,
        lavorazione: Lavorazione::default(),
        ..Default::default()
    };
    match &prn {
        Some(out) => {
            for id in &annidamento.esclusi {
                let d = &documenti[*id as usize];
                eprintln!(
                    "attenzione: {} è escluso dal nesting e non verrà scritto con --prn",
                    d.percorso.display()
                );
            }
            let primo = annidamento
                .fogli
                .first()
                .context("nessun foglio da scrivere: nessun documento è entrato nel nesting")?;
            let mut dest = std::fs::File::create(out)?;
            stampante::scrivi_job(&mut dest, primo.file.percorso(), Formato::Pdf, &base, |_| {})?;
            println!(
                "primo foglio ({} documenti) → {}",
                primo.documenti.len(),
                out.display()
            );
        }
        None => {
            for foglio in &annidamento.fogli {
                stampante::invia(
                    &config.host,
                    config.porta,
                    foglio.file.percorso(),
                    Formato::Pdf,
                    &base,
                    |_| {},
                )?;
            }
            println!(
                "{} fogli, {} pezzi → {}:{}",
                annidamento.fogli.len(),
                annidamento.pezzi,
                config.host,
                config.porta
            );
            // I documenti che il nesting non ha preso partono uno alla volta.
            for id in &annidamento.esclusi {
                let d = &documenti[*id as usize];
                let info = file::analizza(&d.percorso)?;
                let opz = OpzioniJob {
                    nome: d.percorso.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                    copie,
                    sorgente,
                    scala_di_grigi,
                    impaginazione: None,
                    lavorazione,
                    ..Default::default()
                };
                stampante::invia(
                    &config.host,
                    config.porta,
                    &d.percorso,
                    d.formato,
                    &opz,
                    |_| {},
                )?;
                println!(
                    "{} → {}:{} ({})",
                    d.percorso.display(),
                    config.host,
                    config.porta,
                    info.dettagli
                );
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
        None => println!(
            "Rotolo:   non rilevato (l'ingresso del rotolo non dichiara la larghezza, o SNMP non raggiungibile)"
        ),
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn valida_scala_controlla_anche_le_pagine_successive() {
        let prima = (210.0, 297.0);
        let mut info = file::InfoFile {
            formato: Formato::Pdf,
            byte: 0,
            pagine: 3,
            mm: Some(prima),
            risoluzione: None,
            dettagli: String::new(),
            formati_pagine: Vec::new(),
            pagine_mm: vec![Some(prima), None, Some((10.0, 10.0))],
        };
        let lav = Lavorazione {
            scala: Scala::Formato(lavorazione::FormatoIso::A4),
            ..Default::default()
        };
        let percorso = std::path::Path::new("formati-misti.pdf");
        assert!(valida_scala(percorso, &info, &lav).is_err());
        // Anche con la prima pagina priva di misure, le successive vanno validate.
        info.mm = None;
        info.pagine_mm[0] = None;
        assert!(valida_scala(percorso, &info, &lav).is_err());
        info.pagine_mm[2] = Some((148.0, 210.0));
        assert!(valida_scala(percorso, &info, &lav).is_ok());
        // Il ripiego sulle misure della prima pagina rimane valido.
        info.pagine_mm.clear();
        info.mm = Some((10.0, 10.0));
        assert!(valida_scala(percorso, &info, &lav).is_err());
    }
}
