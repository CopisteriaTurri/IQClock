//! Riconoscimento dei file trascinati e informazioni per la lista.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::pjl::Formato;

#[derive(Clone, Debug)]
pub struct InfoFile {
    pub formato: Formato,
    pub byte: u64,
    pub pagine: usize,
    /// Dimensioni della prima pagina in mm (per le immagini: supponendo 300 dpi).
    pub mm: Option<(f64, f64)>,
    /// Es. "3 pagine · 594 × 841 mm (A1)" oppure "4961 × 7016 px".
    pub dettagli: String,
}

/// Risoluzione presunta delle immagini senza DPI noti, per l'anteprima sul rotolo.
pub const DPI_IMMAGINI: f64 = 300.0;

pub fn riconosci(percorso: &Path) -> Result<Formato> {
    let mut testa = [0u8; 8];
    let letti = File::open(percorso)?.read(&mut testa)?;
    let testa = &testa[..letti];
    let est = percorso
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    Ok(if testa.starts_with(b"%PDF") {
        Formato::Pdf
    } else if testa.starts_with(b"%!") {
        Formato::PostScript
    } else if testa.starts_with(b"II*\0") || testa.starts_with(b"MM\0*") {
        Formato::Tiff
    } else if testa.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Formato::Jpeg
    } else if matches!(est.as_str(), "plt" | "hpgl" | "hpg" | "hp2" | "prn") {
        Formato::Hpgl2
    } else {
        bail!("formato non supportato (servono PDF, PS, TIFF, JPEG o HP-GL/2)")
    })
}

pub fn analizza(percorso: &Path) -> Result<InfoFile> {
    let formato = riconosci(percorso)?;
    let byte = std::fs::metadata(percorso)?.len();
    let mut info = InfoFile { formato, byte, pagine: 1, mm: None, dettagli: String::new() };
    match formato {
        Formato::Pdf => match leggi_pdf(percorso) {
            Ok((pagine, mm)) => {
                info.pagine = pagine;
                info.mm = mm;
                info.dettagli =
                    if pagine == 1 { "1 pagina".to_string() } else { format!("{pagine} pagine") };
                if let Some((l, a)) = mm {
                    info.dettagli.push_str(&format!(" · {l:.0} × {a:.0} mm{}", formato_iso(l, a)));
                }
            }
            Err(e) => info.dettagli = format!("PDF illeggibile: {e}"),
        },
        Formato::Tiff | Formato::Jpeg => {
            if let Ok(d) = imagesize::size(percorso) {
                let px_mm = 25.4 / DPI_IMMAGINI;
                info.mm = Some((d.width as f64 * px_mm, d.height as f64 * px_mm));
                info.dettagli = format!("{} × {} px", d.width, d.height);
            }
        }
        Formato::PostScript | Formato::Hpgl2 => {}
    }
    Ok(info)
}

/// Numero di pagine e dimensioni della prima pagina.
fn leggi_pdf(percorso: &Path) -> Result<(usize, Option<(f64, f64)>)> {
    let doc = lopdf::Document::load(percorso).context("apertura")?;
    let pagine = doc.get_pages();
    let mm = pagine.values().next().and_then(|&id| dimensioni_pagina_mm(&doc, id));
    Ok((pagine.len(), mm))
}

/// MediaBox (anche ereditata dal nodo padre) in mm, tenendo conto di /Rotate.
fn dimensioni_pagina_mm(doc: &lopdf::Document, pagina: lopdf::ObjectId) -> Option<(f64, f64)> {
    let mut nodo = doc.get_dictionary(pagina).ok()?;
    let mut ruota = None;
    loop {
        if ruota.is_none() {
            ruota = nodo.get(b"Rotate").ok().and_then(|o| o.as_i64().ok());
        }
        if let Ok(mb) = nodo.get(b"MediaBox").and_then(|o| o.as_array()) {
            let v: Vec<f64> = mb.iter().filter_map(|o| o.as_float().ok().map(f64::from)).collect();
            if v.len() != 4 {
                return None;
            }
            let pt_mm = 25.4 / 72.0;
            let (l, a) = ((v[2] - v[0]).abs() * pt_mm, (v[3] - v[1]).abs() * pt_mm);
            return Some(if ruota.unwrap_or(0).rem_euclid(180) == 90 { (a, l) } else { (l, a) });
        }
        let padre = nodo.get(b"Parent").ok()?.as_reference().ok()?;
        nodo = doc.get_dictionary(padre).ok()?;
    }
}

/// " (A1)" se le misure corrispondono a un formato ISO A, con 2 mm di tolleranza.
fn formato_iso(l: f64, a: f64) -> String {
    const ISO: [(&str, f64, f64); 6] = [
        ("A0", 841.0, 1189.0),
        ("A1", 594.0, 841.0),
        ("A2", 420.0, 594.0),
        ("A3", 297.0, 420.0),
        ("A4", 210.0, 297.0),
        ("A5", 148.0, 210.0),
    ];
    let (corto, lungo) = if l < a { (l, a) } else { (a, l) };
    ISO.iter()
        .find(|(_, c, g)| (corto - c).abs() < 2.0 && (lungo - g).abs() < 2.0)
        .map(|(n, ..)| format!(" ({n})"))
        .unwrap_or_default()
}

pub fn dimensione_leggibile(byte: u64) -> String {
    match byte {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn iso_con_tolleranza() {
        assert_eq!(formato_iso(594.3, 840.9), " (A1)");
        assert_eq!(formato_iso(841.0, 594.0), " (A1)");
        assert_eq!(formato_iso(600.0, 900.0), "");
    }

    #[test]
    fn riconosce_dai_byte() {
        let dir = std::env::temp_dir().join(format!("iqclock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let casi: [(&str, &[u8], Option<Formato>); 5] = [
            ("a.bin", b"%PDF-1.7\n", Some(Formato::Pdf)),
            ("b.bin", b"%!PS-Adobe-3.0", Some(Formato::PostScript)),
            ("c.bin", b"II*\0xxxx", Some(Formato::Tiff)),
            ("d.plt", b"IN;SP1;", Some(Formato::Hpgl2)),
            ("e.txt", b"ciao", None),
        ];
        for (nome, dati, atteso) in casi {
            let p = dir.join(nome);
            std::fs::write(&p, dati).unwrap();
            assert_eq!(riconosci(&p).ok(), atteso, "{nome}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
