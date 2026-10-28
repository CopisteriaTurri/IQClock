//! Riconoscimento dei file trascinati e informazioni per la lista.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::pjl::Formato;
use crate::png;

#[derive(Clone, Debug)]
pub struct InfoFile {
    pub formato: Formato,
    pub byte: u64,
    pub pagine: usize,
    /// Dimensioni della prima pagina in mm (per le immagini: dai DPI
    /// dichiarati nel file, altrimenti supponendo 300 dpi).
    pub mm: Option<(f64, f64)>,
    /// DPI usati per le misure delle immagini (JPEG, TIFF, PNG);
    /// nessuno per gli altri formati.
    pub dpi: Option<(f64, f64)>,
    /// Vero se i DPI sono i 300 presunti (nel file mancano o sono assurdi).
    pub dpi_presunti: bool,
    /// Es. "3 pagine · 594 × 841 mm (A1)" oppure "4961 × 7016 px · 300 dpi".
    pub dettagli: String,
}

/// Risoluzione presunta delle immagini senza DPI noti, per l'anteprima sul rotolo.
pub const DPI_IMMAGINI: f64 = 300.0;

/// Intervallo di DPI dichiarati credibili: sotto (0, 1, ...) sono valori
/// assurdi o rapporti d'aspetto, sopra la risoluzione massima della macchina.
const DPI_MIN: f64 = 10.0;
const DPI_MAX: f64 = 2400.0;

/// Tiene solo i DPI dichiarati credibili, altrimenti `None` (ricadere sui presunti).
pub fn dpi_sensati(dpi: Option<(f64, f64)>) -> Option<(f64, f64)> {
    let (x, y) = dpi?;
    if [x, y].iter().all(|d| d.is_finite() && (DPI_MIN..=DPI_MAX).contains(d)) {
        Some((x, y))
    } else {
        None
    }
}

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
    } else if testa.starts_with(png::FIRMA) {
        Formato::Png
    } else if matches!(est.as_str(), "plt" | "hpgl" | "hpg" | "hp2" | "prn") {
        Formato::Hpgl2
    } else {
        bail!("formato non supportato (servono PDF, PNG, PS, TIFF, JPEG o HP-GL/2)")
    })
}

pub fn analizza(percorso: &Path) -> Result<InfoFile> {
    let formato = riconosci(percorso)?;
    let byte = std::fs::metadata(percorso)?.len();
    let mut info = InfoFile { formato, byte, pagine: 1, mm: None, dpi: None, dpi_presunti: false, dettagli: String::new() };
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
        Formato::Tiff | Formato::Jpeg | Formato::Png => {
            if let Ok(d) = imagesize::size(percorso) {
                // I PNG dichiarano i DPI nel chunk pHYs, i JPEG in EXIF/JFIF,
                // i TIFF nei tag di risoluzione; se mancano o sono assurdi
                // si presumono 300 dpi.
                let dichiarati = dpi_sensati(match formato {
                    Formato::Png => png::dpi(percorso),
                    Formato::Jpeg => dpi_jpeg(percorso),
                    _ => dpi_tiff(percorso),
                });
                let (dpi_x, dpi_y, presunti) = match dichiarati {
                    Some((x, y)) => (x, y, false),
                    None => (DPI_IMMAGINI, DPI_IMMAGINI, true),
                };
                info.mm = Some((d.width as f64 * 25.4 / dpi_x, d.height as f64 * 25.4 / dpi_y));
                info.dpi = Some((dpi_x, dpi_y));
                info.dpi_presunti = presunti;
                info.dettagli = dettagli_immagine(d.width, d.height, dpi_x, dpi_y, presunti);
            }
        }
        Formato::PostScript | Formato::Hpgl2 => {}
    }
    Ok(info)
}

/// "4961 × 7016 px · 300 dpi" (o "300 dpi (presunti)" se stimati).
fn dettagli_immagine(l: usize, a: usize, dpi_x: f64, dpi_y: f64, presunti: bool) -> String {
    let dpi = if (dpi_x - dpi_y).abs() < 0.5 {
        format!("{dpi_x:.0} dpi")
    } else {
        format!("{dpi_x:.0} × {dpi_y:.0} dpi")
    };
    if presunti {
        format!("{l} × {a} px · {dpi} (presunti)")
    } else {
        format!("{l} × {a} px · {dpi}")
    }
}

/// DPI dichiarati da un JPEG: prima EXIF, altrimenti densità JFIF.
fn dpi_jpeg(percorso: &Path) -> Option<(f64, f64)> {
    // I DPI stanno nei marker iniziali (APP0/APP1): basta l'inizio del file.
    let mut dati = Vec::new();
    File::open(percorso).ok()?.take(256 * 1024).read_to_end(&mut dati).ok()?;
    dpi_da_jpeg(&dati)
}

fn dpi_da_jpeg(dati: &[u8]) -> Option<(f64, f64)> {
    if dati.len() < 4 || !dati.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut jfif = None;
    let mut i = 2;
    while i + 4 <= dati.len() {
        if dati[i] != 0xFF {
            break;
        }
        let mut j = i + 1;
        while j < dati.len() && dati[j] == 0xFF {
            j += 1;
        }
        if j >= dati.len() {
            break;
        }
        let marker = dati[j];
        // Marker senza lunghezza; SOS (dati compressi) ed EOI chiudono.
        if marker == 0x01 || marker == 0xD8 || (0xD0..=0xD9).contains(&marker) {
            if marker == 0xD9 {
                break;
            }
            i = j + 1;
            continue;
        }
        if marker == 0xDA || j + 3 > dati.len() {
            break;
        }
        let len = u16::from_be_bytes([dati[j + 1], dati[j + 2]]) as usize;
        if len < 2 || j + 1 + len > dati.len() {
            break;
        }
        let corpo = &dati[j + 3..j + 1 + len];
        match marker {
            0xE0 if jfif.is_none() => jfif = dpi_da_jfif(corpo),
            0xE1 => {
                if let Some(dpi) = dpi_da_exif(corpo) {
                    return Some(dpi);
                }
            }
            _ => {}
        }
        i = j + 1 + len;
    }
    jfif
}

/// Densità dal segmento JFIF (APP0): unità 1 = dpi, 2 = punti per cm,
/// 0 = solo rapporto d'aspetto (non serve a stampare).
fn dpi_da_jfif(corpo: &[u8]) -> Option<(f64, f64)> {
    if corpo.len() < 14 || !corpo.starts_with(b"JFIF\0") {
        return None;
    }
    let (unita, x, y) = (
        corpo[7],
        u16::from_be_bytes([corpo[8], corpo[9]]),
        u16::from_be_bytes([corpo[10], corpo[11]]),
    );
    if x == 0 || y == 0 {
        return None;
    }
    match unita {
        1 => Some((f64::from(x), f64::from(y))),
        2 => Some((f64::from(x) * 2.54, f64::from(y) * 2.54)),
        _ => None,
    }
}

/// Risoluzioni dal segmento EXIF (APP1): prima IFD del TIFF incorporato.
fn dpi_da_exif(corpo: &[u8]) -> Option<(f64, f64)> {
    if corpo.len() < 14 || !corpo.starts_with(b"Exif\0\0") {
        return None;
    }
    let tiff = &corpo[6..];
    let little = match &tiff[..4] {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let off = u32_ordine(&tiff[4..8], little) as usize;
    risoluzioni_da_ifd(tiff, off, little)
}

/// DPI dai tag di risoluzione di un file TIFF (prima IFD).
fn dpi_tiff(percorso: &Path) -> Option<(f64, f64)> {
    // IFD e valori stanno all'inizio anche nei file grossi.
    let mut dati = Vec::new();
    File::open(percorso).ok()?.take(1024 * 1024).read_to_end(&mut dati).ok()?;
    dpi_da_tiff(&dati)
}

fn dpi_da_tiff(dati: &[u8]) -> Option<(f64, f64)> {
    if dati.len() < 8 {
        return None;
    }
    let little = match &dati[..4] {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let off = u32_ordine(&dati[4..8], little) as usize;
    risoluzioni_da_ifd(dati, off, little)
}

fn u16_ordine(b: &[u8], little: bool) -> u16 {
    if little { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) }
}

fn u32_ordine(b: &[u8], little: bool) -> u32 {
    if little {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    } else {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    }
}

/// Legge XResolution/YResolution/ResolutionUnit dalla prima IFD di un header
/// TIFF in memoria (EXIF dentro un JPEG o file TIFF). Unità 2 = inch (dpi),
/// 3 = cm; senza unità si presume inch, come da specifica TIFF.
fn risoluzioni_da_ifd(tiff: &[u8], ifd: usize, little: bool) -> Option<(f64, f64)> {
    let u16a = |p: usize| tiff.get(p..p + 2).map(|b| u16_ordine(b, little));
    let u32a = |p: usize| tiff.get(p..p + 4).map(|b| u32_ordine(b, little));
    let razionale = |p: usize| {
        let (n, d) = (u32a(p)?, u32a(p + 4)?);
        if d == 0 { None } else { Some(n as f64 / d as f64) }
    };
    let n = u16a(ifd)? as usize;
    let (mut x, mut y) = (None, None);
    let mut unita = 2u16;
    for k in 0..n {
        let e = ifd.checked_add(2)?.checked_add(k.checked_mul(12)?)?;
        let (tag, tipo, conto) = (u16a(e)?, u16a(e + 2)?, u32a(e + 4)?);
        let campo = u32a(e + 8)?;
        match tag {
            0x011A | 0x011B if tipo == 5 && conto >= 1 => {
                let p = campo as usize;
                if p.checked_add(8)? > tiff.len() {
                    continue;
                }
                if tag == 0x011A {
                    x = razionale(p).or(x);
                } else {
                    y = razionale(p).or(y);
                }
            }
            // SHORT: il valore occupa i primi 2 byte del campo.
            0x0128 if tipo == 3 && conto >= 1 => {
                unita = if little { (campo & 0xFFFF) as u16 } else { (campo >> 16) as u16 };
            }
            _ => {}
        }
    }
    match (x?, y?, unita) {
        (x, y, 2) => Some((x, y)),
        (x, y, 3) => Some((x * 2.54, y * 2.54)),
        _ => None,
    }
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
        let casi: [(&str, &[u8], Option<Formato>); 6] = [
            ("a.bin", b"%PDF-1.7\n", Some(Formato::Pdf)),
            ("b.bin", b"%!PS-Adobe-3.0", Some(Formato::PostScript)),
            ("c.bin", b"II*\0xxxx", Some(Formato::Tiff)),
            ("d.plt", b"IN;SP1;", Some(Formato::Hpgl2)),
            ("e.png", png::FIRMA, Some(Formato::Png)),
            ("f.txt", b"ciao", None),
        ];
        for (nome, dati, atteso) in casi {
            let p = dir.join(nome);
            std::fs::write(&p, dati).unwrap();
            assert_eq!(riconosci(&p).ok(), atteso, "{nome}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// JPEG minimo con solo APP0 JFIF (150 dpi) e fine file.
    fn jpeg_con_jfif(unita: u8, x: u16, y: u16) -> Vec<u8> {
        let mut app0 = b"JFIF\0".to_vec();
        app0.extend_from_slice(&[1, 2, unita]);
        app0.extend_from_slice(&x.to_be_bytes());
        app0.extend_from_slice(&y.to_be_bytes());
        app0.extend_from_slice(&[0, 0, 0]); // nessuna miniatura
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpeg.extend_from_slice(&((app0.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&app0);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        jpeg
    }

    /// IFD TIFF little-endian con X/YResolution e ResolutionUnit.
    fn ifd_tiff(x_num: u32, x_den: u32, y_num: u32, y_den: u32, unita: u16) -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes()); // prima IFD subito dopo
        t.extend_from_slice(&3u16.to_le_bytes());
        let raz_x = 8 + 2 + 3 * 12 + 4; // dopo header e IFD: 50
        let raz_y = raz_x + 8;
        let voce = |tag: u16, tipo: u16, conto: u32, valore: u32| {
            let mut v = tag.to_le_bytes().to_vec();
            v.extend_from_slice(&tipo.to_le_bytes());
            v.extend_from_slice(&conto.to_le_bytes());
            v.extend_from_slice(&valore.to_le_bytes());
            v
        };
        t.extend_from_slice(&voce(0x011A, 5, 1, raz_x));
        t.extend_from_slice(&voce(0x011B, 5, 1, raz_y));
        t.extend_from_slice(&voce(0x0128, 3, 1, u32::from(unita)));
        t.extend_from_slice(&0u32.to_le_bytes()); // niente altre IFD
        assert_eq!(t.len() as u32, raz_x);
        for (n, d) in [(x_num, x_den), (y_num, y_den)] {
            t.extend_from_slice(&n.to_le_bytes());
            t.extend_from_slice(&d.to_le_bytes());
        }
        t
    }

    /// JPEG minimo con APP1 EXIF (header TIFF + IFD passati).
    fn jpeg_con_exif(tiff: &[u8]) -> Vec<u8> {
        let mut corpo = b"Exif\0\0".to_vec();
        corpo.extend_from_slice(tiff);
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((corpo.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&corpo);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        jpeg
    }

    #[test]
    fn dpi_jpeg_da_jfif() {
        assert_eq!(dpi_da_jpeg(&jpeg_con_jfif(1, 150, 150)), Some((150.0, 150.0)));
        // Punti per cm: 59 dpcm ≈ 150 dpi.
        let (x, _) = dpi_da_jpeg(&jpeg_con_jfif(2, 59, 59)).unwrap();
        assert!((x - 150.0).abs() < 0.5, "{x}");
        // Unità 0 = solo rapporto d'aspetto: niente DPI.
        assert_eq!(dpi_da_jpeg(&jpeg_con_jfif(0, 150, 150)), None);
        // Densità zero: valore assurdo.
        assert_eq!(dpi_da_jpeg(&jpeg_con_jfif(1, 0, 150)), None);
        // Non un JPEG.
        assert_eq!(dpi_da_jpeg(b"%PDF-1.7\n"), None);
    }

    #[test]
    fn dpi_jpeg_da_exif() {
        let tiff = ifd_tiff(150, 1, 150, 1, 2);
        assert_eq!(dpi_da_jpeg(&jpeg_con_exif(&tiff)), Some((150.0, 150.0)));
        // EXIF vince su JFIF (qui 72 dpi).
        let mut jpeg = jpeg_con_jfif(1, 72, 72);
        jpeg.pop(); // toglie EOI
        let exif = jpeg_con_exif(&tiff);
        jpeg.extend_from_slice(&exif[2..]);
        assert_eq!(dpi_da_jpeg(&jpeg), Some((150.0, 150.0)));
        // Unità in cm: 59/1 dpcm ≈ 150 dpi.
        let tiff_cm = ifd_tiff(59, 1, 59, 1, 3);
        let (x, y) = dpi_da_jpeg(&jpeg_con_exif(&tiff_cm)).unwrap();
        assert!((x - 150.0).abs() < 0.5 && (y - 150.0).abs() < 0.5, "{x} {y}");
    }

    #[test]
    fn dpi_tiff_dai_tag() {
        assert_eq!(dpi_da_tiff(&ifd_tiff(150, 1, 150, 1, 2)), Some((150.0, 150.0)));
        // Anche risoluzioni intere diverse (72 dpi) sono plausibili.
        assert_eq!(dpi_da_tiff(&ifd_tiff(72, 1, 72, 1, 2)), Some((72.0, 72.0)));
        // Denominatore zero: tag inutilizzabile.
        assert_eq!(dpi_da_tiff(&ifd_tiff(150, 0, 150, 1, 2)), None);
        assert_eq!(dpi_da_tiff(b"II*\0xxxx"), None);
    }

    #[test]
    fn dpi_assurdi_ricadono_sui_presunti() {
        assert_eq!(dpi_sensati(None), None);
        assert_eq!(dpi_sensati(Some((0.0, 0.0))), None);
        assert_eq!(dpi_sensati(Some((1.0, 1.0))), None);
        assert_eq!(dpi_sensati(Some((100_000.0, 100_000.0))), None);
        assert_eq!(dpi_sensati(Some((72.0, 72.0))), Some((72.0, 72.0)));
        assert_eq!(dpi_sensati(Some((300.0, 300.0))), Some((300.0, 300.0)));
    }

    #[test]
    fn analizza_tiff_con_dpi_dichiarati() {
        use tiff::encoder::{Rational, TiffEncoder, colortype};
        use tiff::tags::ResolutionUnit;

        let dir = std::env::temp_dir().join(format!("iqclock-dpi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("foto.tif");
        {
            let mut enc = TiffEncoder::new(std::fs::File::create(&p).unwrap()).unwrap();
            let mut img = enc.new_image::<colortype::RGB8>(4, 2).unwrap();
            img.resolution_unit(ResolutionUnit::Inch);
            img.x_resolution(Rational { n: 150, d: 1 });
            img.y_resolution(Rational { n: 150, d: 1 });
            img.write_data(&[10u8; 24]).unwrap();
        }

        let info = analizza(&p).unwrap();
        assert_eq!(info.dpi, Some((150.0, 150.0)));
        assert!(!info.dpi_presunti);
        assert_eq!(info.dettagli, "4 × 2 px · 150 dpi");
        let (l, a) = info.mm.unwrap();
        assert!((l - 4.0 * 25.4 / 150.0).abs() < 0.01 && (a - 2.0 * 25.4 / 150.0).abs() < 0.01);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn analizza_png_senza_dpi_presume_300() {
        use image::{ImageBuffer, Rgb};

        let dir = std::env::temp_dir().join(format!("iqclock-nodpi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("foto.png");
        ImageBuffer::from_pixel(2, 1, Rgb([10u8, 20, 30])).save(&p).unwrap();

        let info = analizza(&p).unwrap();
        assert_eq!(info.dpi, Some((DPI_IMMAGINI, DPI_IMMAGINI)));
        assert!(info.dpi_presunti);
        assert_eq!(info.dettagli, "2 × 1 px · 300 dpi (presunti)");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
