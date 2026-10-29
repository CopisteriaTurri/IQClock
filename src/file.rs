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
    /// Dimensioni della prima pagina in mm (per le immagini: ricavate dai DPI
    /// dichiarati dal file, o da quelli presunti se mancano).
    pub mm: Option<(f64, f64)>,
    /// DPI usati per le immagini e se sono presunti; `None` per gli altri formati.
    pub risoluzione: Option<Risoluzione>,
    /// Es. "3 pagine · 594 × 841 mm (A1)" oppure
    /// "4961 × 7016 px · 300 dpi (presunti) · 420 × 594 mm (A2)".
    pub dettagli: String,
}

/// Risoluzione usata per un'immagine, in punti per pollice.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Risoluzione {
    pub x: f64,
    pub y: f64,
    /// true se il file non dichiarava DPI utilizzabili e si sono usati quelli presunti.
    pub presunta: bool,
}

impl Risoluzione {
    /// "300 dpi (presunti)", "300 dpi" oppure "150 × 300 dpi".
    pub fn etichetta(&self) -> String {
        if self.presunta {
            format!("{} dpi (presunti)", valore_dpi(self.x))
        } else if (self.x - self.y).abs() < 0.5 {
            format!("{} dpi", valore_dpi(self.x))
        } else {
            format!("{} × {} dpi", valore_dpi(self.x), valore_dpi(self.y))
        }
    }
}

fn valore_dpi(dpi: f64) -> String {
    if (dpi - dpi.round()).abs() < 0.05 { format!("{dpi:.0}") } else { format!("{dpi:.1}") }
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
    let mut info =
        InfoFile { formato, byte, pagine: 1, mm: None, risoluzione: None, dettagli: String::new() };
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
                let (x, y, presunta) = match risoluzione_dichiarata(percorso, formato) {
                    Some((x, y)) => (x, y, false),
                    None => (DPI_IMMAGINI, DPI_IMMAGINI, true),
                };
                let risoluzione = Risoluzione { x, y, presunta };
                let (l, a) = (d.width as f64 * 25.4 / x, d.height as f64 * 25.4 / y);
                info.mm = Some((l, a));
                info.dettagli = format!(
                    "{} × {} px · {} · {l:.0} × {a:.0} mm{}",
                    d.width,
                    d.height,
                    risoluzione.etichetta(),
                    formato_iso(l, a)
                );
                info.risoluzione = Some(risoluzione);
            }
        }
        Formato::PostScript | Formato::Hpgl2 => {}
    }
    Ok(info)
}

/// DPI dichiarati dal file, ripuliti da valori assenti o assurdi (0, 1, ecc.).
fn risoluzione_dichiarata(percorso: &Path, formato: Formato) -> Option<(f64, f64)> {
    let (x, y) = match formato {
        // Il PNG dichiara i DPI nel chunk pHYs.
        Formato::Png => png::dpi(percorso)?,
        Formato::Jpeg | Formato::Tiff => {
            // Bastano le prime centinaia di KB: EXIF e IFD stanno all'inizio del file.
            let mut testa = Vec::new();
            File::open(percorso).ok()?.take(256 * 1024).read_to_end(&mut testa).ok()?;
            if formato == Formato::Jpeg { dpi_jpeg(&testa)? } else { risoluzione_tiff(&testa)? }
        }
        _ => return None,
    };
    dpi_plausibili(x, y)
}

/// Scarta i DPI che non possono essere veri: 0, 1 e valori fuori scala.
pub(crate) fn dpi_plausibili(x: f64, y: f64) -> Option<(f64, f64)> {
    const MINIMO: f64 = 2.0;
    const MASSIMO: f64 = 10_000.0;
    ((MINIMO..=MASSIMO).contains(&x) && (MINIMO..=MASSIMO).contains(&y)).then_some((x, y))
}

/// DPI dichiarati da un JPEG: prima EXIF (APP1), poi JFIF (APP0).
fn dpi_jpeg(testa: &[u8]) -> Option<(f64, f64)> {
    if !testa.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let (mut exif, mut jfif) = (None, None);
    let mut i = 2;
    while i + 1 < testa.len() {
        if testa[i] != 0xFF {
            return None;
        }
        // Prima del marcatore possono esserci più 0xFF di riempimento.
        let mut j = i + 1;
        while testa.get(j) == Some(&0xFF) {
            j += 1;
        }
        let marcatore = *testa.get(j)?;
        if marcatore == 0x01 || (0xD0..=0xD8).contains(&marcatore) {
            // Marcatori senza lunghezza.
            i = j + 1;
            continue;
        }
        if marcatore == 0xDA || marcatore == 0xD9 {
            // Da SOS in poi non ci sono più segmenti APPn.
            break;
        }
        let lunghezza = be16(testa, j + 1)? as usize;
        if lunghezza < 2 {
            return None;
        }
        let dati = testa.get(j + 3..j + 1 + lunghezza)?;
        match marcatore {
            0xE0 => jfif = jfif.or_else(|| dpi_jfif(dati).and_then(|(x, y)| dpi_plausibili(x, y))),
            0xE1 => {
                if let Some(contenuto) = dati.strip_prefix(b"Exif\0\0") {
                    exif = exif.or_else(|| {
                        risoluzione_tiff(contenuto).and_then(|(x, y)| dpi_plausibili(x, y))
                    });
                }
            }
            _ => {}
        }
        i = j + 1 + lunghezza;
    }
    exif.or(jfif)
}

/// Densità JFIF (APP0): unità 1 = dpi, 2 = punti per cm, 0 = solo proporzioni.
fn dpi_jfif(dati: &[u8]) -> Option<(f64, f64)> {
    if !dati.starts_with(b"JFIF\0") {
        return None;
    }
    let unita = *dati.get(7)?;
    let x = be16(dati, 8)? as f64;
    let y = be16(dati, 10)? as f64;
    match unita {
        1 => Some((x, y)),
        2 => Some((x * 2.54, y * 2.54)),
        _ => None,
    }
}

/// XResolution/YResolution/ResolutionUnit dell'IFD0 di un TIFF (o dell'EXIF
/// dentro un JPEG), convertite in dpi.
fn risoluzione_tiff(dati: &[u8]) -> Option<(f64, f64)> {
    let ordine = match dati.get(..2)? {
        b"II" => Ordine::Piccolo,
        b"MM" => Ordine::Grande,
        _ => return None,
    };
    if ordine.u16(dati, 2)? != 42 {
        return None;
    }
    let ifd = ordine.u32(dati, 4)? as usize;
    let voci = ordine.u16(dati, ifd)? as usize;
    let (mut x, mut y) = (None, None);
    // Se manca, TIFF 6.0 presuppone i pollici.
    let mut unita = 2u16;
    for k in 0..voci {
        let voce = ifd + 2 + k * 12;
        let tag = ordine.u16(dati, voce)?;
        let tipo = ordine.u16(dati, voce + 2)?;
        let valore = ordine.u32(dati, voce + 8)? as usize;
        match tag {
            282 => x = razionale(dati, ordine, tipo, valore),
            283 => y = razionale(dati, ordine, tipo, valore),
            296 if tipo == 3 => unita = ordine.u16(dati, voce + 8)?,
            _ => {}
        }
    }
    let fattore = match unita {
        2 => 1.0,         // pollici
        3 => 2.54,        // centimetri
        _ => return None, // unità relativa: i valori non sono DPI
    };
    Some((x? * fattore, y? * fattore))
}

/// Valore RATIONAL (o SRATIONAL) puntato da una voce dell'IFD.
fn razionale(dati: &[u8], ordine: Ordine, tipo: u16, offset: usize) -> Option<f64> {
    let (num, den) = match tipo {
        5 => (ordine.u32(dati, offset)? as f64, ordine.u32(dati, offset + 4)? as f64),
        10 => (ordine.u32(dati, offset)? as i32 as f64, ordine.u32(dati, offset + 4)? as i32 as f64),
        _ => return None,
    };
    (den != 0.0).then_some(num / den)
}

fn be16(dati: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes(dati.get(i..i + 2)?.try_into().ok()?))
}

#[derive(Clone, Copy)]
enum Ordine {
    Piccolo,
    Grande,
}

impl Ordine {
    fn u16(self, dati: &[u8], i: usize) -> Option<u16> {
        let b: [u8; 2] = dati.get(i..i + 2)?.try_into().ok()?;
        Some(match self {
            Ordine::Piccolo => u16::from_le_bytes(b),
            Ordine::Grande => u16::from_be_bytes(b),
        })
    }

    fn u32(self, dati: &[u8], i: usize) -> Option<u32> {
        let b: [u8; 4] = dati.get(i..i + 4)?.try_into().ok()?;
        Some(match self {
            Ordine::Piccolo => u32::from_le_bytes(b),
            Ordine::Grande => u32::from_be_bytes(b),
        })
    }
}

/// Numero di pagine e dimensioni della prima pagina.
fn leggi_pdf(percorso: &Path) -> Result<(usize, Option<(f64, f64)>)> {
    let doc = lopdf::Document::load(percorso).context("apertura")?;
    let pagine = doc.get_pages();
    if pagine.is_empty() && doc.is_encrypted() {
        bail!("cifrato (serve la password)");
    }
    let mm = pagine.values().next().and_then(|&id| dimensioni_pagina_mm(&doc, id));
    Ok((pagine.len(), mm))
}

/// MediaBox e /Rotate (entrambi ereditabili) in mm, risalendo l'albero delle
/// pagine: vince il valore più vicino alla pagina.
fn dimensioni_pagina_mm(doc: &lopdf::Document, pagina: lopdf::ObjectId) -> Option<(f64, f64)> {
    let mut nodo = doc.get_dictionary(pagina).ok()?;
    let mut ruota = None;
    let mut lati = None;
    // Un albero di pagine è profondo pochi livelli: il limite evita di girare
    // all'infinito su un PDF con /Parent ciclici.
    for _ in 0..32 {
        if ruota.is_none() {
            ruota = nodo.get(b"Rotate").ok().and_then(|o| o.as_i64().ok());
        }
        if lati.is_none()
            && let Ok(mb) = nodo.get(b"MediaBox").and_then(|o| o.as_array())
        {
            let v: Vec<f64> =
                mb.iter().filter_map(|o| o.as_float().ok().map(f64::from)).collect();
            if v.len() != 4 {
                return None;
            }
            lati = Some(((v[2] - v[0]).abs(), (v[3] - v[1]).abs()));
        }
        if ruota.is_some() && lati.is_some() {
            break;
        }
        let Some(padre) = nodo.get(b"Parent").ok().and_then(|o| o.as_reference().ok()) else {
            break;
        };
        let Ok(padre) = doc.get_dictionary(padre) else { break };
        nodo = padre;
    }
    let (l, a) = lati?;
    let pt_mm = 25.4 / 72.0;
    let (l, a) = (l * pt_mm, a * pt_mm);
    Some(if ruota.unwrap_or(0).rem_euclid(180) == 90 { (a, l) } else { (l, a) })
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
    use std::path::PathBuf;

    use lopdf::{Dictionary, Document, Object, Stream};

    // ---- file di prova ----

    fn cartella(nome: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iqclock-file-{}-{nome}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scrivi(dir: &Path, nome: &str, dati: &[u8]) -> PathBuf {
        let p = dir.join(nome);
        std::fs::write(&p, dati).unwrap();
        p
    }

    /// Punti tipografici corrispondenti a `mm` millimetri.
    fn pt(mm: f64) -> f64 {
        mm * 72.0 / 25.4
    }

    #[derive(Clone, Copy)]
    struct PaginaTest {
        /// Larghezza e altezza del MediaBox in punti; `None` = ereditata.
        media_box: Option<(f64, f64)>,
        ruota: Option<i64>,
    }

    fn pagina(l: f64, a: f64) -> PaginaTest {
        PaginaTest { media_box: Some((l, a)), ruota: None }
    }

    fn media_box((l, a): (f64, f64)) -> Object {
        Object::Array([0.0, 0.0, l, a].iter().map(|v| Object::Real(*v as f32)).collect())
    }

    /// PDF minimo ma valido, con pagina o MediaBox ereditato dal nodo `Pages`.
    fn pdf(pagine: &[PaginaTest], media_box_padre: Option<(f64, f64)>) -> Vec<u8> {
        pdf_con_padre(pagine, media_box_padre, None)
    }

    fn pdf_con_padre(
        pagine: &[PaginaTest],
        media_box_padre: Option<(f64, f64)>,
        ruota_padre: Option<i64>,
    ) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let fogli_id = doc.new_object_id();
        let contenuto = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));

        let mut fogli = Vec::new();
        for p in pagine {
            let mut d = Dictionary::new();
            d.set("Type", "Page");
            d.set("Parent", Object::Reference(fogli_id));
            d.set("Contents", Object::Reference(contenuto));
            d.set("Resources", Dictionary::new());
            if let Some(mb) = p.media_box {
                d.set("MediaBox", media_box(mb));
            }
            if let Some(r) = p.ruota {
                d.set("Rotate", r);
            }
            fogli.push(Object::Reference(doc.add_object(d)));
        }

        let mut albero = Dictionary::new();
        albero.set("Type", "Pages");
        albero.set("Kids", Object::Array(fogli));
        albero.set("Count", pagine.len() as i64);
        if let Some(mb) = media_box_padre {
            albero.set("MediaBox", media_box(mb));
        }
        if let Some(r) = ruota_padre {
            albero.set("Rotate", r);
        }
        doc.objects.insert(fogli_id, Object::Dictionary(albero));

        let mut catalogo = Dictionary::new();
        catalogo.set("Type", "Catalog");
        catalogo.set("Pages", Object::Reference(fogli_id));
        let catalogo_id = doc.add_object(catalogo);
        doc.trailer.set("Root", catalogo_id);

        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn analizza_bytes(dir: &Path, nome: &str, dati: &[u8]) -> InfoFile {
        analizza(&scrivi(dir, nome, dati)).unwrap()
    }

    // ---- riconosci ----

    #[test]
    fn riconosce_dai_byte_ignorando_l_estensione() {
        let dir = cartella("byte");
        let casi: [(&str, &[u8], Option<Formato>); 9] = [
            ("a.pdf", b"%PDF-1.7\n", Some(Formato::Pdf)),
            ("b.pdf", b"%!PS-Adobe-3.0", Some(Formato::PostScript)),
            ("c.pdf", b"II*\0xxxx", Some(Formato::Tiff)),
            ("d.pdf", b"MM\0*xxxx", Some(Formato::Tiff)),
            ("e.pdf", &[0xFF, 0xD8, 0xFF, 0xE0], Some(Formato::Jpeg)),
            ("f.plt", b"IN;SP1;", Some(Formato::Hpgl2)),
            ("g.txt", b"ciao", None),
            ("h.pdf", b"", None),
            ("i.pdf", png::FIRMA, Some(Formato::Png)),
        ];
        for (nome, dati, atteso) in casi {
            let p = scrivi(&dir, nome, dati);
            assert_eq!(riconosci(&p).ok(), atteso, "{nome}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn riconosce_hpgl_dall_estensione() {
        let dir = cartella("hpgl");
        for est in ["plt", "hpgl", "hpg", "hp2", "prn", "PLT", "HpGl"] {
            let p = scrivi(&dir, &format!("disegno.{est}"), b"IN;SP1;");
            assert_eq!(riconosci(&p).ok(), Some(Formato::Hpgl2), "{est}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rifiuta_file_mancanti_e_sconosciuti() {
        let dir = cartella("rifiuti");
        let mancante = dir.join("non-esiste.pdf");
        assert!(riconosci(&mancante).is_err());

        let ignoto = scrivi(&dir, "dati.bin", b"\x00\x01\x02\x03");
        let err = riconosci(&ignoto).unwrap_err().to_string();
        assert!(err.contains("formato non supportato"), "{err}");
        assert!(analizza(&ignoto).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- PDF ----

    #[test]
    fn pdf_a4() {
        let dir = cartella("pdf-a4");
        let info = analizza_bytes(&dir, "a4.pdf", &pdf(&[pagina(pt(210.0), pt(297.0))], None));
        assert_eq!(info.formato, Formato::Pdf);
        assert_eq!(info.pagine, 1);
        assert_eq!(info.dettagli, "1 pagina · 210 × 297 mm (A4)");
        let (l, a) = info.mm.unwrap();
        assert!((l - 210.0).abs() < 0.5 && (a - 297.0).abs() < 0.5, "{l} × {a}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_formati_iso() {
        let dir = cartella("pdf-iso");
        let formati = [
            ("A0", 841.0, 1189.0),
            ("A1", 594.0, 841.0),
            ("A2", 420.0, 594.0),
            ("A3", 297.0, 420.0),
            ("A4", 210.0, 297.0),
            ("A5", 148.0, 210.0),
        ];
        for (nome, l, a) in formati {
            let p = scrivi(&dir, &format!("{nome}.pdf"), &pdf(&[pagina(pt(l), pt(a))], None));
            let info = analizza(&p).unwrap();
            assert_eq!(info.dettagli, format!("1 pagina · {l:.0} × {a:.0} mm ({nome})"), "{nome}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_formato_custom() {
        let dir = cartella("pdf-custom");
        let info = analizza_bytes(&dir, "custom.pdf", &pdf(&[pagina(800.0, 1200.0)], None));
        // 800 × 1200 pt = 282 × 423 mm: nessun formato ISO.
        assert_eq!(info.dettagli, "1 pagina · 282 × 423 mm");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_ruotato_scambia_larghezza_e_altezza() {
        let dir = cartella("pdf-ruotato");
        let mut p = pagina(pt(210.0), pt(297.0));
        p.ruota = Some(90);
        let info = analizza_bytes(&dir, "ruotato.pdf", &pdf(&[p], None));
        assert_eq!(info.dettagli, "1 pagina · 297 × 210 mm (A4)");

        // 180° non scambia le misure.
        let mut dritto = pagina(pt(210.0), pt(297.0));
        dritto.ruota = Some(180);
        let info = analizza_bytes(&dir, "capovolto.pdf", &pdf(&[dritto], None));
        assert_eq!(info.dettagli, "1 pagina · 210 × 297 mm (A4)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_rotazione_ereditata_dal_padre() {
        let dir = cartella("pdf-rotazione-ereditata");
        let info = analizza_bytes(
            &dir,
            "padre90.pdf",
            &pdf_con_padre(&[pagina(pt(210.0), pt(297.0))], None, Some(90)),
        );
        assert_eq!(info.dettagli, "1 pagina · 297 × 210 mm (A4)");

        // La rotazione più vicina alla pagina ha la precedenza.
        let mut figlio = pagina(pt(210.0), pt(297.0));
        figlio.ruota = Some(180);
        let info = analizza_bytes(
            &dir,
            "figlio180.pdf",
            &pdf_con_padre(&[figlio], None, Some(90)),
        );
        assert_eq!(info.dettagli, "1 pagina · 210 × 297 mm (A4)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_rotazione_e_media_box_entrambi_ereditati() {
        let dir = cartella("pdf-padre-completo");
        let senza_niente = PaginaTest { media_box: None, ruota: None };
        let info = analizza_bytes(
            &dir,
            "padre.pdf",
            &pdf_con_padre(&[senza_niente], Some((pt(210.0), pt(297.0))), Some(-90)),
        );
        assert_eq!(info.dettagli, "1 pagina · 297 × 210 mm (A4)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_media_box_ereditato_dal_padre() {
        let dir = cartella("pdf-ereditato");
        let senza_media_box = PaginaTest { media_box: None, ruota: None };
        let info = analizza_bytes(
            &dir,
            "ereditato.pdf",
            &pdf(&[senza_media_box], Some((pt(594.0), pt(841.0)))),
        );
        assert_eq!(info.dettagli, "1 pagina · 594 × 841 mm (A1)");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Riscrive il /Parent della prima pagina; `None` = la pagina stessa.
    fn con_parent(dati: &[u8], padre: Option<lopdf::ObjectId>) -> Vec<u8> {
        let mut doc = Document::load_mem(dati).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        let d = doc.get_object_mut(id).unwrap().as_dict_mut().unwrap();
        d.set("Parent", Object::Reference(padre.unwrap_or(id)));
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn pdf_parent_mancante_tiene_il_media_box_della_pagina() {
        let dir = cartella("pdf-parent-rotto");
        let dati = con_parent(&pdf(&[pagina(pt(210.0), pt(297.0))], None), Some((999, 0)));
        let info = analizza_bytes(&dir, "rotto.pdf", &dati);
        assert_eq!(info.dettagli, "1 pagina · 210 × 297 mm (A4)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_parent_ciclico_non_si_blocca() {
        let dir = cartella("pdf-parent-ciclico");
        let dati = con_parent(&pdf(&[pagina(pt(210.0), pt(297.0))], None), None);
        let info = analizza_bytes(&dir, "ciclo.pdf", &dati);
        assert_eq!(info.dettagli, "1 pagina · 210 × 297 mm (A4)");

        let senza_media_box = PaginaTest { media_box: None, ruota: None };
        let dati = con_parent(&pdf(&[senza_media_box], None), None);
        let info = analizza_bytes(&dir, "ciclo-vuoto.pdf", &dati);
        assert_eq!(info.mm, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_multipagina_usa_la_prima() {
        let dir = cartella("pdf-pagine");
        let pagine = [
            pagina(pt(594.0), pt(841.0)),
            pagina(pt(210.0), pt(297.0)),
            pagina(pt(420.0), pt(594.0)),
        ];
        let info = analizza_bytes(&dir, "tre.pdf", &pdf(&pagine, None));
        assert_eq!(info.pagine, 3);
        assert_eq!(info.dettagli, "3 pagine · 594 × 841 mm (A1)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_senza_pagine() {
        let dir = cartella("pdf-vuoto");
        let info = analizza_bytes(&dir, "vuoto.pdf", &pdf(&[], None));
        assert_eq!(info.pagine, 0);
        assert_eq!(info.mm, None);
        assert_eq!(info.dettagli, "0 pagine");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_corrotto_finisce_nei_dettagli() {
        let dir = cartella("pdf-rotto");
        let info = analizza_bytes(&dir, "rotto.pdf", b"%PDF-1.7\nquesto non e un pdf");
        assert!(info.dettagli.starts_with("PDF illeggibile: "), "{}", info.dettagli);
        assert_eq!(info.pagine, 1);
        assert_eq!(info.mm, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_cifrato_resta_leggibile_nella_lista() {
        let dir = cartella("pdf-cifrato");
        let mut doc = Document::load_mem(&pdf(&[pagina(pt(210.0), pt(297.0))], None)).unwrap();
        let mut cifratura = Dictionary::new();
        cifratura.set("Filter", "Standard");
        cifratura.set("V", 1);
        cifratura.set("R", 2);
        cifratura.set("O", Object::String(vec![0; 32], lopdf::StringFormat::Hexadecimal));
        cifratura.set("U", Object::String(vec![0; 32], lopdf::StringFormat::Hexadecimal));
        cifratura.set("P", -4);
        let cifratura = doc.add_object(cifratura);
        doc.trailer.set("Encrypt", Object::Reference(cifratura));
        let mut dati = Vec::new();
        doc.save_to(&mut dati).unwrap();

        let info = analizza_bytes(&dir, "cifrato.pdf", &dati);
        assert_eq!(info.formato, Formato::Pdf);
        // Senza password le pagine non sono leggibili: meglio dirlo che
        // mostrare "0 pagine".
        assert_eq!(info.dettagli, "PDF illeggibile: cifrato (serve la password)");
        assert_eq!(info.mm, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- immagini e altri formati ----

    #[test]
    fn immagini_jpeg_tiff_e_png() {
        let dir = cartella("immagini");
        let rgb = image::RgbImage::from_pixel(40, 30, image::Rgb([10, 200, 10]));
        for (nome, formato, atteso) in [
            ("foto.jpg", image::ImageFormat::Jpeg, Formato::Jpeg),
            ("scansione.tif", image::ImageFormat::Tiff, Formato::Tiff),
            ("foto.png", image::ImageFormat::Png, Formato::Png),
        ] {
            let p = dir.join(nome);
            rgb.save_with_format(&p, formato).unwrap();
            assert_eq!(riconosci(&p).unwrap(), atteso, "{nome}");
            let info = analizza(&p).unwrap();
            assert_eq!(info.formato, atteso, "{nome}");
            assert_eq!(info.pagine, 1, "{nome}");
            assert_eq!(info.dettagli, "40 × 30 px · 300 dpi (presunti) · 3 × 3 mm", "{nome}");
            assert_eq!(
                info.risoluzione,
                Some(Risoluzione { x: 300.0, y: 300.0, presunta: true }),
                "{nome}"
            );
            let (l, a) = info.mm.unwrap();
            // Senza DPI noti si assume DPI_IMMAGINI (300).
            assert!((l - 40.0 * 25.4 / DPI_IMMAGINI).abs() < 0.01, "{nome}: {l}");
            assert!((a - 30.0 * 25.4 / DPI_IMMAGINI).abs() < 0.01, "{nome}: {a}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// TIFF minimo (little endian) con dimensioni e risoluzione nell'IFD0.
    fn tiff_le(px_l: u32, px_a: u32, dpi_x: u32, dpi_y: u32, unita: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"II");
        v.extend_from_slice(&42u16.to_le_bytes());
        v.extend_from_slice(&8u32.to_le_bytes()); // offset dell'IFD0
        v.extend_from_slice(&5u16.to_le_bytes());
        let mut voce = |tag: u16, tipo: u16, valore: u32| {
            v.extend_from_slice(&tag.to_le_bytes());
            v.extend_from_slice(&tipo.to_le_bytes());
            v.extend_from_slice(&1u32.to_le_bytes());
            v.extend_from_slice(&valore.to_le_bytes());
        };
        voce(256, 3, px_l); // ImageWidth
        voce(257, 3, px_a); // ImageLength
        voce(282, 5, 74); // XResolution: RATIONAL a 74
        voce(283, 5, 82); // YResolution: RATIONAL a 82
        voce(296, 3, u32::from(unita)); // ResolutionUnit
        v.extend_from_slice(&0u32.to_le_bytes()); // nessun IFD successivo
        v.extend_from_slice(&dpi_x.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&dpi_y.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v
    }

    /// JPEG minimo con un segmento JFIF.
    fn jpeg_jfif(unita: u8, dpi_x: u16, dpi_y: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.extend_from_slice(&16u16.to_be_bytes());
        v.extend_from_slice(b"JFIF\0\x01\x02");
        v.push(unita);
        v.extend_from_slice(&dpi_x.to_be_bytes());
        v.extend_from_slice(&dpi_y.to_be_bytes());
        v.extend_from_slice(&[0, 0, 0xFF, 0xD9]); // miniatura 0 × 0, EOI
        v
    }

    /// JPEG minimo con un segmento EXIF.
    fn jpeg_exif(dpi: u32, unita: u16) -> Vec<u8> {
        let tiff = tiff_le(300, 300, dpi, dpi, unita);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1];
        v.extend_from_slice(&(app1.len() as u16 + 2).to_be_bytes());
        v.extend_from_slice(&app1);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn dpi_da_tiff() {
        assert_eq!(risoluzione_tiff(&tiff_le(300, 300, 300, 300, 2)), Some((300.0, 300.0)));
    }

    #[test]
    fn dpi_tiff_in_centimetri() {
        let (x, y) = risoluzione_tiff(&tiff_le(300, 300, 118, 118, 3)).unwrap();
        assert!((x - 299.72).abs() < 0.01 && (y - 299.72).abs() < 0.01);
    }

    #[test]
    fn dpi_assenti_o_assurdi() {
        // Unità 1 = solo proporzioni: non sono DPI.
        assert_eq!(risoluzione_tiff(&tiff_le(300, 300, 300, 300, 1)), None);
        assert_eq!(dpi_plausibili(0.0, 0.0), None);
        assert_eq!(dpi_plausibili(1.0, 1.0), None);
        assert_eq!(dpi_plausibili(300.0, 300.0), Some((300.0, 300.0)));
    }

    #[test]
    fn dpi_da_jfif() {
        assert_eq!(dpi_jpeg(&jpeg_jfif(1, 300, 300)), Some((300.0, 300.0)));
        let (x, y) = dpi_jpeg(&jpeg_jfif(2, 118, 118)).unwrap();
        assert!((x - 299.72).abs() < 0.01 && (y - 299.72).abs() < 0.01);
        // Unità 0: solo rapporto d'aspetto, si ricade sui 300 presunti.
        assert_eq!(dpi_jpeg(&jpeg_jfif(0, 1, 1)), None);
    }

    #[test]
    fn dpi_da_exif() {
        assert_eq!(dpi_jpeg(&jpeg_exif(150, 2)), Some((150.0, 150.0)));
    }

    #[test]
    fn etichette_dpi() {
        let r = |x, y, presunta| Risoluzione { x, y, presunta };
        assert_eq!(r(300.0, 300.0, true).etichetta(), "300 dpi (presunti)");
        assert_eq!(r(300.0, 300.0, false).etichetta(), "300 dpi");
        assert_eq!(r(150.0, 300.0, false).etichetta(), "150 × 300 dpi");
    }

    #[test]
    fn analizza_usa_i_dpi_del_file() {
        let dir = cartella("dpi-file");

        // 300 × 150 px a 150 dpi = 50,8 × 25,4 mm.
        let info = analizza_bytes(&dir, "dichiarati.tif", &tiff_le(300, 150, 150, 150, 2));
        assert_eq!(info.risoluzione, Some(Risoluzione { x: 150.0, y: 150.0, presunta: false }));
        let (l, a) = info.mm.unwrap();
        assert!((l - 50.8).abs() < 0.1 && (a - 25.4).abs() < 0.1);
        assert!(info.dettagli.contains("150 dpi"), "{}", info.dettagli);

        // Unità relativa: DPI assenti, si usano i 300 presunti.
        let info = analizza_bytes(&dir, "presunti.tif", &tiff_le(300, 150, 300, 300, 1));
        assert_eq!(info.risoluzione, Some(Risoluzione { x: 300.0, y: 300.0, presunta: true }));
        let (l, a) = info.mm.unwrap();
        assert!((l - 25.4).abs() < 0.1 && (a - 12.7).abs() < 0.1);
        assert!(info.dettagli.contains("300 dpi (presunti)"), "{}", info.dettagli);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Inserisce un chunk `pHYs` subito dopo IHDR (il CRC non serve a `png::dpi`).
    fn png_con_phys(png: &[u8], pixel_per_metro: u32) -> Vec<u8> {
        let fine_ihdr = 8 + 4 + 4 + 13 + 4;
        assert_eq!(&png[12..16], b"IHDR");
        let mut out = png[..fine_ihdr].to_vec();
        out.extend_from_slice(&9u32.to_be_bytes());
        out.extend_from_slice(b"pHYs");
        out.extend_from_slice(&pixel_per_metro.to_be_bytes());
        out.extend_from_slice(&pixel_per_metro.to_be_bytes());
        out.push(1); // unità: pixel per metro
        out.extend_from_slice(&[0; 4]); // CRC
        out.extend_from_slice(&png[fine_ihdr..]);
        out
    }

    #[test]
    fn analizza_usa_i_phys_del_png() {
        let dir = cartella("png-phys");
        let png = {
            let p = dir.join("foto.png");
            image::RgbImage::from_pixel(300, 150, image::Rgb([1, 2, 3]))
                .save_with_format(&p, image::ImageFormat::Png)
                .unwrap();
            std::fs::read(&p).unwrap()
        };

        // 11811 px/m ≈ 300 dpi: 300 × 150 px = 25,4 × 12,7 mm.
        let p = scrivi(&dir, "300.png", &png_con_phys(&png, 11811));
        let info = analizza(&p).unwrap();
        let r = info.risoluzione.unwrap();
        assert!(!r.presunta && (r.x - 300.0).abs() < 0.01 && (r.y - 300.0).abs() < 0.01);
        let (l, a) = info.mm.unwrap();
        assert!((l - 25.4).abs() < 0.1 && (a - 12.7).abs() < 0.1, "{l} × {a}");

        // pHYs assurdo (1 px/m): si ricade sui 300 presunti.
        let p = scrivi(&dir, "assurdo.png", &png_con_phys(&png, 1));
        let info = analizza(&p).unwrap();
        assert_eq!(info.risoluzione, Some(Risoluzione { x: 300.0, y: 300.0, presunta: true }));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn altre_estensioni_non_ingannano_il_riconoscimento() {
        let dir = cartella("estensioni");
        let rgb = image::RgbImage::from_pixel(4, 4, image::Rgb([0, 0, 0]));
        let jpeg = dir.join("finta.pdf");
        rgb.save_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        assert_eq!(riconosci(&jpeg).unwrap(), Formato::Jpeg);

        let testo = scrivi(&dir, "finto.jpg", b"non sono un'immagine");
        assert!(riconosci(&testo).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hpgl_analizzato_senza_dimensioni() {
        let dir = cartella("hpgl-info");
        let info = analizza_bytes(&dir, "disegno.plt", b"IN;SP1;PA0,0;");
        assert_eq!(info.formato, Formato::Hpgl2);
        assert_eq!(info.pagine, 1);
        assert_eq!(info.mm, None);
        assert!(info.dettagli.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- formato_iso ----

    #[test]
    fn iso_con_tolleranza() {
        for (nome, corto, lungo) in [
            ("A0", 841.0, 1189.0),
            ("A1", 594.0, 841.0),
            ("A2", 420.0, 594.0),
            ("A3", 297.0, 420.0),
            ("A4", 210.0, 297.0),
            ("A5", 148.0, 210.0),
        ] {
            assert_eq!(formato_iso(corto, lungo), format!(" ({nome})"));
            assert_eq!(formato_iso(lungo, corto), format!(" ({nome})"), "{nome} ruotato");
        }
    }

    #[test]
    fn iso_soglia_di_due_millimetri() {
        assert_eq!(formato_iso(594.3, 840.9), " (A1)");
        assert_eq!(formato_iso(595.9, 842.9), " (A1)");
        // Esattamente 2 mm: la tolleranza è esclusa.
        assert_eq!(formato_iso(596.0, 841.0), "");
        assert_eq!(formato_iso(594.0, 843.0), "");
        assert_eq!(formato_iso(600.0, 900.0), "");
        assert_eq!(formato_iso(0.0, 0.0), "");
    }
}
