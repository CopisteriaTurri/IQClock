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
    /// Dimensioni della prima pagina in mm (per le immagini: dai DPI del PNG,
    /// altrimenti supponendo 300 dpi).
    pub mm: Option<(f64, f64)>,
    /// Es. "3 pagine · 594 × 841 mm (A1)" oppure "4961 × 7016 px".
    pub dettagli: String,
    /// Fogli A4 equivalenti delle pagine del PDF, arrotondati per eccesso:
    /// le pagine ISO valgono il loro formato (A3 = 2, A0 = 16, A5 = 0,5), le
    /// altre la loro area. `None` per gli altri formati o se le misure di una
    /// pagina non si leggono.
    pub fogli_a4: Option<u32>,
}

/// Risoluzione presunta delle immagini senza DPI noti, per l'anteprima sul rotolo.
pub const DPI_IMMAGINI: f64 = 300.0;

/// Area di un foglio A4 in mm².
const AREA_A4_MM2: f64 = 210.0 * 297.0;

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
        InfoFile { formato, byte, pagine: 1, mm: None, dettagli: String::new(), fogli_a4: None };
    match formato {
        Formato::Pdf => match leggi_pdf(percorso) {
            Ok(pdf) => {
                info.pagine = pdf.pagine;
                info.mm = pdf.mm;
                info.fogli_a4 = pdf.fogli_a4;
                info.dettagli = if pdf.pagine == 1 {
                    "1 pagina".to_string()
                } else {
                    format!("{} pagine", pdf.pagine)
                };
                if let Some((l, a)) = pdf.mm {
                    info.dettagli.push_str(&format!(" · {l:.0} × {a:.0} mm{}", formato_iso(l, a)));
                }
            }
            Err(e) => info.dettagli = format!("PDF illeggibile: {e}"),
        },
        Formato::Tiff | Formato::Jpeg | Formato::Png => {
            if let Ok(d) = imagesize::size(percorso) {
                // I PNG dichiarano i DPI nel chunk pHYs; per gli altri si presume.
                let dichiarati = if formato == Formato::Png { png::dpi(percorso) } else { None };
                let (dpi_x, dpi_y) = dichiarati.unwrap_or((DPI_IMMAGINI, DPI_IMMAGINI));
                info.mm = Some((d.width as f64 * 25.4 / dpi_x, d.height as f64 * 25.4 / dpi_y));
                info.dettagli = format!("{} × {} px", d.width, d.height);
            }
        }
        Formato::PostScript | Formato::Hpgl2 => {}
    }
    Ok(info)
}

/// Dati del PDF che servono alla lista e all'anteprima.
struct Pdf {
    pagine: usize,
    /// Dimensioni della prima pagina.
    mm: Option<(f64, f64)>,
    fogli_a4: Option<u32>,
}

fn leggi_pdf(percorso: &Path) -> Result<Pdf> {
    let doc = lopdf::Document::load(percorso).context("apertura")?;
    let pagine = doc.get_pages();
    if pagine.is_empty() && doc.is_encrypted() {
        bail!("cifrato (serve la password)");
    }
    let mut mm = None;
    let mut misure = Vec::with_capacity(pagine.len());
    let mut tutte_note = true;
    for (n, &id) in pagine.values().enumerate() {
        let pagina = dimensioni_pagina_mm(&doc, id);
        if n == 0 {
            mm = pagina;
        }
        match pagina {
            Some(m) => misure.push(m),
            None => tutte_note = false,
        }
    }
    // Basta una pagina senza misure per rendere il conto inaffidabile.
    let fogli_a4 = (tutte_note && !misure.is_empty()).then(|| conta_fogli_a4(&misure, false));
    Ok(Pdf { pagine: pagine.len(), mm, fogli_a4 })
}

/// Formati ISO A: nome, lati in mm (corto e lungo) e fogli A4 equivalenti.
const ISO: [(&str, f64, f64, f64); 6] = [
    ("A0", 841.0, 1189.0, 16.0),
    ("A1", 594.0, 841.0, 8.0),
    ("A2", 420.0, 594.0, 4.0),
    ("A3", 297.0, 420.0, 2.0),
    ("A4", 210.0, 297.0, 1.0),
    ("A5", 148.0, 210.0, 0.5),
];

/// Formato ISO A corrispondente alle misure (2 mm di tolleranza), con i suoi
/// fogli A4 equivalenti.
fn iso(l: f64, a: f64) -> Option<(&'static str, f64)> {
    let (corto, lungo) = if l < a { (l, a) } else { (a, l) };
    ISO.iter()
        .find(|(_, c, g, _)| (corto - c).abs() < 2.0 && (lungo - g).abs() < 2.0)
        .map(|(n, _, _, fogli)| (*n, *fogli))
}

/// Fogli A4 che servono per le pagine date, in mm. Le pagine ISO valgono il
/// loro formato nominale, le altre la loro area divisa per quella di un A4
/// (arrotondata al mezzo foglio più vicino, perché una pagina appena fuori
/// formato non deve raddoppiare il conto). Con `fronte_retro` due pagine
/// stanno su un foglio. Il totale è arrotondato per eccesso: mezzo foglio è
/// comunque un foglio da prendere.
pub fn conta_fogli_a4(pagine_mm: &[(f64, f64)], fronte_retro: bool) -> u32 {
    let mut fogli = 0.0;
    for &(l, a) in pagine_mm {
        fogli += match iso(l, a) {
            Some((_, equivalenti)) => equivalenti,
            None => (l * a / AREA_A4_MM2 * 2.0).round() / 2.0,
        };
    }
    if fronte_retro {
        fogli /= 2.0;
    }
    // Il rumore dei float non deve trasformare 3 in 4.
    (fogli - 1e-9).ceil().max(0.0) as u32
}

/// "1 foglio A4" oppure "3 fogli A4".
pub fn etichetta_fogli_a4(fogli: u32) -> String {
    if fogli == 1 { "1 foglio A4".into() } else { format!("{fogli} fogli A4") }
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
    iso(l, a).map(|(n, _)| format!(" ({n})")).unwrap_or_default()
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
        assert_eq!(info.fogli_a4, Some(1));
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
            let fogli = match nome {
                "A0" => 16,
                "A1" => 8,
                "A2" => 4,
                "A3" => 2,
                "A4" | "A5" => 1,
                _ => unreachable!(),
            };
            assert_eq!(info.fogli_a4, Some(fogli), "{nome}");
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
        assert_eq!(info.fogli_a4, Some(1));

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
        // Senza le misure di una pagina il conteggio dei fogli non è affidabile.
        assert_eq!(info.fogli_a4, None);
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
        // A1 (8) + A4 (1) + A2 (4).
        assert_eq!(info.fogli_a4, Some(13));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_senza_pagine() {
        let dir = cartella("pdf-vuoto");
        let info = analizza_bytes(&dir, "vuoto.pdf", &pdf(&[], None));
        assert_eq!(info.pagine, 0);
        assert_eq!(info.mm, None);
        assert_eq!(info.dettagli, "0 pagine");
        assert_eq!(info.fogli_a4, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_corrotto_finisce_nei_dettagli() {
        let dir = cartella("pdf-rotto");
        let info = analizza_bytes(&dir, "rotto.pdf", b"%PDF-1.7\nquesto non e un pdf");
        assert!(info.dettagli.starts_with("PDF illeggibile: "), "{}", info.dettagli);
        assert_eq!(info.pagine, 1);
        assert_eq!(info.mm, None);
        assert_eq!(info.fogli_a4, None);
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
        assert_eq!(info.fogli_a4, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- fogli A4 ----

    #[test]
    fn pdf_multipagina_conta_i_fogli_a4() {
        let dir = cartella("pdf-fogli-a4");
        let tre_a4 = [pagina(pt(210.0), pt(297.0)); 3];
        let info = analizza_bytes(&dir, "tre-a4.pdf", &pdf(&tre_a4, None));
        assert_eq!(info.fogli_a4, Some(3));

        // A3 (2) + A1 (8) + A5 (mezzo) = 10,5 → 11 fogli.
        let miste = [
            pagina(pt(297.0), pt(420.0)),
            pagina(pt(594.0), pt(841.0)),
            pagina(pt(148.0), pt(210.0)),
        ];
        let info = analizza_bytes(&dir, "miste.pdf", &pdf(&miste, None));
        assert_eq!(info.pagine, 3);
        assert_eq!(info.fogli_a4, Some(11));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_fuori_formato_conta_per_area() {
        let dir = cartella("pdf-fogli-area");
        // 420 × 420 mm = 2,83 fogli A4, arrotondati a 3; con due A4 fanno 5.
        let pagine = [
            pagina(pt(420.0), pt(420.0)),
            pagina(pt(210.0), pt(297.0)),
            pagina(pt(210.0), pt(297.0)),
        ];
        let info = analizza_bytes(&dir, "area.pdf", &pdf(&pagine, None));
        assert_eq!(info.fogli_a4, Some(5));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn misure_appena_fuori_formato_non_gonfiano_il_conteggio() {
        // 2 mm oltre l'A4: un solo foglio, non due.
        assert_eq!(conta_fogli_a4(&[(212.0, 299.0)], false), 1);
        assert_eq!(conta_fogli_a4(&[(210.0, 297.0)], false), 1);
        // Sensibilmente più grande: due fogli.
        assert_eq!(conta_fogli_a4(&[(240.0, 340.0)], false), 2);
        assert_eq!(conta_fogli_a4(&[], false), 0);
    }

    #[test]
    fn fronte_retro_mette_due_pagine_per_foglio() {
        assert_eq!(conta_fogli_a4(&[(210.0, 297.0); 4], false), 4);
        assert_eq!(conta_fogli_a4(&[(210.0, 297.0); 4], true), 2);
        // Numero dispari: l'ultima pagina prende comunque un foglio.
        assert_eq!(conta_fogli_a4(&[(210.0, 297.0); 3], true), 2);
        // A3 + A1 = 10 fogli a faccia singola, 5 fronte/retro.
        let grandi = [(297.0, 420.0), (594.0, 841.0)];
        assert_eq!(conta_fogli_a4(&grandi, false), 10);
        assert_eq!(conta_fogli_a4(&grandi, true), 5);
    }

    #[test]
    fn etichetta_dei_fogli() {
        assert_eq!(etichetta_fogli_a4(1), "1 foglio A4");
        assert_eq!(etichetta_fogli_a4(0), "0 fogli A4");
        assert_eq!(etichetta_fogli_a4(12), "12 fogli A4");
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
            assert_eq!(info.dettagli, "40 × 30 px", "{nome}");
            assert_eq!(info.fogli_a4, None, "{nome}");
            let (l, a) = info.mm.unwrap();
            // Senza DPI noti si assume DPI_IMMAGINI (300).
            assert!((l - 40.0 * 25.4 / DPI_IMMAGINI).abs() < 0.01, "{nome}: {l}");
            assert!((a - 30.0 * 25.4 / DPI_IMMAGINI).abs() < 0.01, "{nome}: {a}");
        }
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
        assert_eq!(info.fogli_a4, None);
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
