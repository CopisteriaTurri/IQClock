//! Nesting: più pagine disposte sullo stesso foglio del rotolo.
//!
//! Il problema è il classico "shelf packing" (FFDH, First Fit Decreasing
//! Height): ogni pezzo viene orientato, i più alti vanno per primi e ognuno
//! entra nella prima fila dove c'è spazio in larghezza; quando una fila non
//! basta se ne apre una nuova sotto, a `distanza` mm. Le misure sono in mm,
//! `x` parte dal bordo sinistro e `y` dall'alto del foglio.
//!
//! La composizione fa un PDF per foglio (un job PJL per foglio, vedi
//! `stampante.rs`): le pagine dei PDF diventano Form XObject (importando gli
//! oggetti dal documento sorgente, come nell'esempio "merge" di lopdf), i JPEG
//! restano JPEG (`/Filter /DCTDecode`, nessuna ricompressione) e TIFF/PNG
//! arrivano già decodificati in RGB 8 bit compresso Flate.
//!
//! Le annotazioni delle pagine PDF non passano nel Form XObject: si perdono
//! (scritto anche nel README). Il limite di `ALTEZZA_MAX_MM` viene dal fatto che
//! molte implementazioni PDF rifiutano pagine più alte di 14400 pt (5080 mm);
//! non è verificato se la T2600 ne accetta di più.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom};

use anyhow::{Context, Result, bail};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::file;
use crate::pjl::Formato;

const MM_PT: f64 = 72.0 / 25.4;
/// Tolleranza sulle misure in mm: assorbe gli arrotondamenti pt → mm.
const TOLLERANZA_MM: f64 = 1e-6;

/// Altezza massima di un foglio. Oltre i 14400 pt (5080 mm) molte
/// implementazioni PDF non accettano la pagina: 5000 mm resta sotto.
/// Non verificato sulla T2600 se ne accetta di più.
pub const ALTEZZA_MAX_MM: f64 = 5000.0;

/// Opzioni del nesting (sezione NESTING della GUI e riga di comando).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Nesting {
    /// Spazio tra i pezzi e tra le file, in mm.
    pub distanza_mm: f64,
    /// true se i pezzi si possono sdraiare per stare nel rotolo.
    pub ruota: bool,
}

impl Default for Nesting {
    fn default() -> Self {
        Self { distanza_mm: 5.0, ruota: true }
    }
}

/// Un pezzo da mettere sul rotolo: `id` è (documento, pagina, copia), le misure
/// sono quelle finali (dopo scala e cornice), in mm.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pezzo {
    pub id: (u64, usize, u32),
    pub l: f64,
    pub a: f64,
}

/// Dove finisce un pezzo: angolo in alto a sinistra del foglio, in mm.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Posto {
    pub id: (u64, usize, u32),
    pub x: f64,
    pub y: f64,
    pub ruotato: bool,
}

/// Un foglio del rotolo: i pezzi e la sua lunghezza in mm.
#[derive(Clone, Debug, PartialEq)]
pub struct Foglio {
    pub altezza: f64,
    pub posti: Vec<Posto>,
}

/// Formati che il nesting sa comporre; gli altri partono da soli come oggi.
pub fn annidabile(formato: Formato) -> bool {
    !matches!(formato, Formato::PostScript | Formato::Hpgl2)
}

/// Orientamento scelto per un pezzo: (larghezza, altezza, ruotato) oppure
/// `None` se non entra nel rotolo in nessun verso.
fn orienta(pezzo: &Pezzo, larghezza: f64, ruota: bool) -> Option<(f64, f64, bool)> {
    if ruota {
        // Sdraiato: il lato più corto in altezza, così il foglio consuma meno.
        let (corto, lungo) = if pezzo.l <= pezzo.a { (pezzo.l, pezzo.a) } else { (pezzo.a, pezzo.l) };
        if lungo <= larghezza + TOLLERANZA_MM {
            // Girato solo se la pagina era verticale (lato corto in larghezza).
            return Some((lungo, corto, pezzo.l < pezzo.a));
        }
    }
    (pezzo.l <= larghezza + TOLLERANZA_MM).then_some((pezzo.l, pezzo.a, false))
}

/// true se il pezzo entra nella larghezza del rotolo (in qualche verso, se
/// `ruota` è vero).
pub fn entra(pezzo: &Pezzo, larghezza: f64, ruota: bool) -> bool {
    orienta(pezzo, larghezza, ruota).is_some()
}

struct Orientato {
    id: (u64, usize, u32),
    larghezza: f64,
    altezza: f64,
    ruotato: bool,
}

struct Riga {
    y: f64,
    altezza: f64,
    usata: f64,
}

/// Shelf packing FFDH: restituisce i fogli, nell'ordine in cui si susseguono
/// sul rotolo. I pezzi che non entrano nella larghezza vengono lasciati fuori
/// (vedi [`entra`]).
pub fn disponi(
    pezzi: &[Pezzo],
    larghezza: f64,
    distanza: f64,
    ruota: bool,
    altezza_max: f64,
) -> Vec<Foglio> {
    let larghezza = larghezza.max(0.0);
    let distanza = distanza.max(0.0);
    let mut orientati: Vec<Orientato> = pezzi
        .iter()
        .filter_map(|p| {
            orienta(p, larghezza, ruota).map(|(larghezza, altezza, ruotato)| Orientato {
                id: p.id,
                larghezza,
                altezza,
                ruotato,
            })
        })
        .collect();
    // Deve restare stabile: i pezzi della stessa altezza mantengono l'ordine.
    orientati.sort_by(|a, b| b.altezza.partial_cmp(&a.altezza).unwrap_or(Ordering::Equal));

    let mut fogli = Vec::new();
    let mut posti = Vec::new();
    let mut righe: Vec<Riga> = Vec::new();
    let mut fondo = 0.0f64;
    for pezzo in orientati {
        let mut messo = false;
        for riga in &mut righe {
            let x = if riga.usata > 0.0 { riga.usata + distanza } else { 0.0 };
            if x + pezzo.larghezza <= larghezza + TOLLERANZA_MM {
                posti.push(Posto { id: pezzo.id, x, y: riga.y, ruotato: pezzo.ruotato });
                riga.usata = x + pezzo.larghezza;
                riga.altezza = riga.altezza.max(pezzo.altezza);
                fondo = fondo.max(riga.y + riga.altezza);
                messo = true;
                break;
            }
        }
        if messo {
            continue;
        }
        // La nuova fila parte sotto il fondo di tutte quelle già aperte.
        let mut nuova = if righe.is_empty() { 0.0 } else { fondo + distanza };
        // Un nuovo foglio solo se c'è già qualcosa sopra: un pezzo più alto di
        // `altezza_max` da solo non ha alternative.
        if !righe.is_empty() && nuova + pezzo.altezza > altezza_max + TOLLERANZA_MM {
            fogli.push(Foglio { altezza: fondo, posti: std::mem::take(&mut posti) });
            righe.clear();
            fondo = 0.0;
            nuova = 0.0;
        }
        righe.push(Riga { y: nuova, altezza: pezzo.altezza, usata: pezzo.larghezza });
        posti.push(Posto { id: pezzo.id, x: 0.0, y: nuova, ruotato: pezzo.ruotato });
        fondo = fondo.max(nuova + pezzo.altezza);
    }
    if !posti.is_empty() {
        fogli.push(Foglio { altezza: fondo, posti });
    }
    fogli
}

/// Contenuto di un documento annidato: per un PDF è il documento intero (le
/// pagine si scelgono per indice), per un'immagine l'immagine già pronta.
pub enum Contenuto {
    Pdf(Vec<u8>),
    /// Byte JPEG così come sono: nessuna ricompressione.
    Jpeg(Vec<u8>),
    /// Pixel RGB 8 bit, già su bianco.
    Rgb { larghezza: u32, altezza: u32, pixel: Vec<u8> },
}

/// Un documento da annidare: il suo contenuto e le misure finali di ogni
/// pagina in mm (per le immagini una sola).
pub struct Sorgente {
    pub id: u64,
    pub contenuto: Contenuto,
    pub misure: Vec<(f64, f64)>,
}

/// Compone un PDF per ogni foglio, largo quanto la larghezza utile del rotolo.
pub fn componi(fogli: &[Foglio], sorgenti: &[Sorgente], larghezza_mm: f64) -> Result<Vec<Vec<u8>>> {
    fogli.iter().map(|f| componi_foglio(f, sorgenti, larghezza_mm)).collect()
}

/// Risorsa già creata per una pagina: nome nel dizionario XObject e sue
/// dimensioni nello spazio del Form (punti), che servono alla matrice.
struct Risorsa {
    nome: String,
    larghezza: f64,
    altezza: f64,
}

fn componi_foglio(foglio: &Foglio, sorgenti: &[Sorgente], larghezza_mm: f64) -> Result<Vec<u8>> {
    let mut doc = Document::with_version("1.5");
    let id_pagine = doc.new_object_id();
    let mut xobjects = Dictionary::new();
    let mut contenuti = String::new();
    // (documento, pagina) → risorsa già creata.
    let mut risorse: HashMap<(u64, usize), Risorsa> = HashMap::new();
    // documenti PDF già importati in questo documento.
    let mut importati: HashMap<u64, Vec<ObjectId>> = HashMap::new();

    for posto in &foglio.posti {
        let sorgente = sorgenti
            .iter()
            .find(|s| s.id == posto.id.0)
            .with_context(|| format!("documento {} sconosciuto al nesting", posto.id.0))?;
        let (l, a) = *sorgente
            .misure
            .get(posto.id.1)
            .with_context(|| format!("pagina {} senza misure", posto.id.1))?;
        let chiave = (sorgente.id, posto.id.1);
        if !risorse.contains_key(&chiave) {
            let (xobject, larghezza, altezza) = crea_oggetto(&mut doc, sorgente, posto, &mut importati)?;
            let nome = format!("IQN{}", risorse.len());
            xobjects.set(nome.clone(), doc.add_object(xobject));
            risorse.insert(chiave, Risorsa { nome, larghezza, altezza });
        }
        let risorsa = &risorse[&chiave];
        let nome = &risorsa.nome;

        // Altezza come viene disegnato sul foglio: un pezzo sdraiato scambia i lati.
        let alt = if posto.ruotato { l } else { a };
        let (w_pt, h_pt) = (risorsa.larghezza, risorsa.altezza);
        let sx = l * MM_PT / w_pt;
        let sy = a * MM_PT / h_pt;
        let x = posto.x * MM_PT;
        let y = (foglio.altezza - posto.y - alt) * MM_PT;
        let matrice = if posto.ruotato {
            // Rotazione di 90° in senso orario attorno all'angolo in basso a
            // sinistra, poi scala sui due assi.
            [0.0, -sx, sy, 0.0, x, y + alt * MM_PT]
        } else {
            [sx, 0.0, 0.0, sy, x, y]
        };
        contenuti.push_str(&format!(
            "q {} {} {} {} {} {} cm /{nome} Do Q\n",
            numero(matrice[0]),
            numero(matrice[1]),
            numero(matrice[2]),
            numero(matrice[3]),
            numero(matrice[4]),
            numero(matrice[5]),
        ));
    }

    let larghezza_pt = larghezza_mm * MM_PT;
    let altezza_pt = foglio.altezza * MM_PT;
    let media: Vec<Object> = [0.0, 0.0, larghezza_pt, altezza_pt]
        .iter()
        .map(|&v| Object::Real(v as f32))
        .collect();
    let contenuto = doc.add_object(Stream::new(Dictionary::new(), contenuti.into_bytes()));
    let pagina = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => id_pagine,
        "Contents" => contenuto,
        "MediaBox" => media.clone(),
        "CropBox" => media,
        "Resources" => dictionary! { "XObject" => xobjects },
    });
    doc.objects.insert(
        id_pagine,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![pagina.into()],
            "Count" => 1,
        }),
    );
    let catalogo = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => id_pagine });
    doc.trailer.set("Root", catalogo);

    let mut out = Vec::new();
    doc.save_to(&mut out).context("scrittura del foglio annidato")?;
    Ok(out)
}

/// Crea l'XObject della pagina e ne restituisce anche le dimensioni visive in
/// punti, che servono alla matrice di disegno.
fn crea_oggetto(
    doc: &mut Document,
    sorgente: &Sorgente,
    posto: &Posto,
    importati: &mut HashMap<u64, Vec<ObjectId>>,
) -> Result<(Stream, f64, f64)> {
    match &sorgente.contenuto {
        Contenuto::Pdf(dati) => {
            if let std::collections::hash_map::Entry::Vacant(e) = importati.entry(sorgente.id) {
                e.insert(importa_pdf(doc, dati)?);
            }
            let pagina = *importati[&sorgente.id]
                .get(posto.id.1)
                .with_context(|| format!("pagina {} assente nel PDF", posto.id.1))?;
            let ([x0, y0, x1, y1], ruota) = file::box_e_rotazione(doc, pagina).context("pagina senza misure")?;
            let (larghezza, altezza) = if ruota % 180 == 90 {
                (y1 - y0, x1 - x0)
            } else {
                (x1 - x0, y1 - y0)
            };
            Ok((form_xobject(doc, pagina)?, larghezza, altezza))
        }
        // Un'Image XObject vive nel quadrato unitario: la matrice la porta
        // direttamente alle misure in punti del pezzo.
        Contenuto::Jpeg(dati) => Ok((immagine_jpeg(dati)?, 1.0, 1.0)),
        Contenuto::Rgb { larghezza, altezza, pixel } => {
            Ok((immagine_rgb(*larghezza, *altezza, pixel.clone()), 1.0, 1.0))
        }
    }
}

/// Importa un PDF nel documento di uscita, come l'esempio "merge" di lopdf:
/// rinumera gli oggetti dopo l'ultimo id usato e li copia. Restituisce gli id
/// delle pagine, nell'ordine delle pagine.
fn importa_pdf(dest: &mut Document, dati: &[u8]) -> Result<Vec<ObjectId>> {
    let mut sorgente = Document::load_mem(dati).context("apertura del PDF da annidare")?;
    if sorgente.is_encrypted() {
        bail!("PDF cifrato: non si può annidare");
    }
    sorgente.renumber_objects_with(dest.max_id + 1);
    let pagine: Vec<ObjectId> = sorgente.get_pages().into_values().collect();
    for (id, oggetto) in sorgente.objects {
        dest.objects.insert(id, oggetto);
    }
    dest.max_id = dest.max_id.max(sorgente.max_id);
    Ok(pagine)
}

/// Pagina di un PDF come Form XObject: box visibile normalizzato a `[0 0 w h]`
/// con la matrice che porta lo spazio utente (compreso `/Rotate`) dentro il box.
fn form_xobject(doc: &Document, pagina: ObjectId) -> Result<Stream> {
    let ([x0, y0, x1, y1], ruota) = file::box_e_rotazione(doc, pagina).context("pagina senza misure")?;
    if ruota % 90 != 0 {
        bail!("pagina ruotata di {ruota}°: non si può annidare");
    }
    let (matrice, ..) = matrice_visuale([x0, y0, x1, y1], ruota);
    let risorse = file::ereditato(doc, pagina, b"Resources")
        .cloned()
        .unwrap_or(Object::Dictionary(Dictionary::new()));
    let contenuto = doc.get_page_content(pagina);
    let mut dizionario = Dictionary::new();
    dizionario.set("Type", "XObject");
    dizionario.set("Subtype", "Form");
    // Il BBox è nello spazio del Form, cioè nelle coordinate del contenuto, e
    // il clip si applica dopo il Matrix: va dato il riquadro della pagina, non
    // quello visivo. Con le misure visive una pagina ruotata di 90/270 gradi
    // perdeva la parte oltre il lato corto (verificato col rendering).
    dizionario.set("BBox", reali(&[x0, y0, x1, y1]));
    dizionario.set("Matrix", reali(&matrice));
    dizionario.set("Resources", risorse);
    Ok(Stream::new(dizionario, contenuto))
}

/// Matrice che porta il riquadro `[x0 y0 x1 y1]` (spazio utente) nel box
/// visivo `[0 0 w h]`, con `/Rotate` applicato; e le misure del box.
fn matrice_visuale([x0, y0, x1, y1]: [f64; 4], ruota: i64) -> ([f64; 6], f64, f64) {
    match ruota.rem_euclid(360) {
        90 => ([0.0, -1.0, 1.0, 0.0, -y0, x1], y1 - y0, x1 - x0),
        180 => ([-1.0, 0.0, 0.0, -1.0, x1, y1], x1 - x0, y1 - y0),
        270 => ([0.0, 1.0, -1.0, 0.0, y1, -x0], y1 - y0, x1 - x0),
        _ => ([1.0, 0.0, 0.0, 1.0, -x0, -y0], x1 - x0, y1 - y0),
    }
}

/// Intestazione di un JPEG: misure, componenti e se è un Adobe CMYK (da
/// invertire con `/Decode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Jpeg {
    pub larghezza: u32,
    pub altezza: u32,
    pub componenti: u8,
    pub precisione: u8,
    pub adobe: bool,
}

/// Legge l'intestazione di un JPEG (fino al primo SOF). Non tocca i dati.
pub fn dimensioni_jpeg(dati: &[u8]) -> Result<Jpeg> {
    let mut lettore = Cursor::new(dati);
    let mut testa = [0u8; 2];
    lettore.read_exact(&mut testa).context("JPEG troncato")?;
    if testa != [0xFF, 0xD8] {
        bail!("non è un JPEG");
    }
    let mut adobe = false;
    loop {
        let Some((marcatore, offset, lunghezza)) = file::prossimo_segmento_jpeg(&mut lettore) else {
            bail!("JPEG senza intestazione SOF");
        };
        match marcatore {
            0xEE => {
                if let Some(dati) = file::leggi_a(&mut lettore, offset, usize::try_from(lunghezza).unwrap_or(0)) {
                    adobe = dati.starts_with(b"Adobe");
                }
            }
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                let sof = file::leggi_a(&mut lettore, offset, 6).context("intestazione SOF troncata")?;
                let altezza = u16::from_be_bytes([sof[1], sof[2]]);
                let larghezza = u16::from_be_bytes([sof[3], sof[4]]);
                if larghezza == 0 || altezza == 0 {
                    bail!("JPEG senza dimensioni");
                }
                return Ok(Jpeg {
                    larghezza: u32::from(larghezza),
                    altezza: u32::from(altezza),
                    componenti: sof[5],
                    precisione: sof[0],
                    adobe,
                });
            }
            _ => {}
        }
        lettore
            .seek(SeekFrom::Start(offset + lunghezza))
            .context("JPEG troncato")?;
    }
}

/// Image XObject da un JPEG: i byte del file entrano così come sono.
fn immagine_jpeg(dati: &[u8]) -> Result<Stream> {
    let info = dimensioni_jpeg(dati)?;
    let spazio = match info.componenti {
        1 => "DeviceGray",
        3 => "DeviceRGB",
        _ => "DeviceCMYK",
    };
    let mut dizionario = Dictionary::new();
    dizionario.set("Type", "XObject");
    dizionario.set("Subtype", "Image");
    dizionario.set("Width", i64::from(info.larghezza));
    dizionario.set("Height", i64::from(info.altezza));
    dizionario.set("ColorSpace", spazio);
    dizionario.set("BitsPerComponent", i64::from(info.precisione));
    dizionario.set("Filter", "DCTDecode");
    // Gli Adobe CMYK sono salvati invertiti: senza /Decode escono in negativo.
    if info.componenti == 4 && info.adobe {
        dizionario.set("Decode", reali(&[1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0]));
    }
    Ok(Stream::new(dizionario, dati.to_vec()))
}

/// Image XObject RGB 8 bit, compresso Flate.
fn immagine_rgb(larghezza: u32, altezza: u32, pixel: Vec<u8>) -> Stream {
    let mut dizionario = Dictionary::new();
    dizionario.set("Type", "XObject");
    dizionario.set("Subtype", "Image");
    dizionario.set("Width", i64::from(larghezza));
    dizionario.set("Height", i64::from(altezza));
    dizionario.set("ColorSpace", "DeviceRGB");
    dizionario.set("BitsPerComponent", 8);
    let mut stream = Stream::new(dizionario, pixel);
    // Se la compressione non conviene resta com'è: va bene lo stesso.
    let _ = stream.compress();
    stream
}

fn reali(valori: &[f64]) -> Vec<Object> {
    valori.iter().map(|&v| Object::Real(v as f32)).collect()
}

/// Numeri nei comandi di contenuto: 4 decimali bastano abbondantemente.
fn numero(v: f64) -> String {
    format!("{v:.4}")
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::file::FileTemporaneo;

    fn pezzo(id: (u64, usize, u32), l: f64, a: f64) -> Pezzo {
        Pezzo { id, l, a }
    }

    /// Rettangolo occupato da un posto, in mm (x, y, larghezza, altezza).
    fn rettangolo(posto: &Posto, pezzi: &[Pezzo]) -> (f64, f64, f64, f64) {
        let p = pezzi.iter().find(|p| p.id == posto.id).unwrap();
        if posto.ruotato { (posto.x, posto.y, p.a, p.l) } else { (posto.x, posto.y, p.l, p.a) }
    }

    fn sovrapposti(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
        let tolleranza = 1e-9;
        a.0 < b.0 + b.2 - tolleranza
            && b.0 < a.0 + a.2 - tolleranza
            && a.1 < b.1 + b.3 - tolleranza
            && b.1 < a.1 + a.3 - tolleranza
    }

    #[test]
    fn quattro_a4_su_un_a4_largo_entrano_in_due_file() {
        // A4 sdraiati: 297 × 210. Tre stanno in 904 (3 × 297 + 2 × 5 = 901),
        // il quarto va sotto: 210 + 5 + 210 = 425 mm.
        let pezzi: Vec<_> = (0..4).map(|i| pezzo((0, i, 0), 210.0, 297.0)).collect();
        let fogli = disponi(&pezzi, 904.0, 5.0, true, ALTEZZA_MAX_MM);
        assert_eq!(fogli.len(), 1);
        assert_eq!(fogli[0].posti.len(), 4);
        assert!((fogli[0].altezza - 425.0).abs() < 1e-9, "{}", fogli[0].altezza);
        assert!(fogli[0].posti.iter().all(|p| p.ruotato));
        let prima: Vec<_> = fogli[0].posti.iter().filter(|p| (p.y - 0.0).abs() < 1e-9).collect();
        assert_eq!(prima.len(), 3);
        assert!((prima[1].x - 302.0).abs() < 1e-9);
        assert!((prima[2].x - 604.0).abs() < 1e-9);
        let seconda = fogli[0].posti.iter().find(|p| (p.y - 215.0).abs() < 1e-9).unwrap();
        assert!((seconda.x - 0.0).abs() < 1e-9);
    }

    #[test]
    fn senza_rotazione_i_pezzi_restano_come_sono() {
        // A4 verticali su 500 mm: due affiancati stanno (2 × 210 + 5 = 425), il
        // terzo va sotto (2 × 297 + 5 = 594).
        let pezzi: Vec<_> = (0..3).map(|i| pezzo((0, i, 0), 210.0, 297.0)).collect();
        let fogli = disponi(&pezzi, 500.0, 5.0, false, ALTEZZA_MAX_MM);
        assert!(fogli[0].posti.iter().all(|p| !p.ruotato));
        assert_eq!(fogli[0].posti.len(), 3);
        // Due file da 297 più i 5 mm tra loro.
        assert!((fogli[0].altezza - 599.0).abs() < 1e-9, "{}", fogli[0].altezza);
    }

    #[test]
    fn un_pezzo_piu_largo_del_rotolo_resta_fuori() {
        let pezzi = [
            pezzo((0, 0, 0), 210.0, 297.0),
            pezzo((1, 0, 0), 1000.0, 100.0),
            pezzo((2, 0, 0), 120.0, 500.0),
        ];
        let fogli = disponi(&pezzi, 904.0, 5.0, true, ALTEZZA_MAX_MM);
        let posti: Vec<_> = fogli.iter().flat_map(|f| &f.posti).map(|p| p.id.0).collect();
        assert!(posti.contains(&0));
        assert!(!posti.contains(&1), "1000 mm non entra in nessun verso");
        // 120 × 500 sdraiato sul lato lungo (500 ≤ 904) ci sta.
        assert!(posti.contains(&2));
        assert!(!entra(&pezzo((9, 0, 0), 1000.0, 904.0), 904.0, true));
        assert!(entra(&pezzo((9, 0, 0), 904.0, 1000.0), 904.0, true));
    }

    /// Nessuna sovrapposizione e tutti dentro la larghezza, su misure
    /// pseudo-casuali (generatore congruenziale lineare, nessuna dipendenza).
    #[test]
    fn disposizione_senza_sovrapposizioni() {
        let mut stato = 12345u64;
        let mut misure = move || {
            stato = stato.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (stato >> 33) as u32
        };
        let pezzi: Vec<_> = (0..200)
            .map(|i| {
                let l = 40.0 + f64::from(misure() % 500);
                let a = 40.0 + f64::from(misure() % 700);
                pezzo((i as u64, 0, 0), l, a)
            })
            .collect();
        let fogli = disponi(&pezzi, 904.0, 5.0, true, 4000.0);
        let tutti: Vec<_> = fogli.iter().flat_map(|f| f.posti.clone()).collect();
        assert_eq!(tutti.len(), pezzi.len());
        for foglio in &fogli {
            assert!(foglio.altezza <= 4000.0 + 1e-6, "foglio troppo lungo: {}", foglio.altezza);
            let rettangoli: Vec<_> = foglio.posti.iter().map(|p| rettangolo(p, &pezzi)).collect();
            for (i, r) in rettangoli.iter().enumerate() {
                assert!(r.0 >= -1e-9, "{r:?}");
                assert!(r.0 + r.2 <= 904.0 + 1e-6, "fuori larghezza: {r:?}");
                assert!(r.1 >= -1e-9, "{r:?}");
                assert!(r.1 + r.3 <= foglio.altezza + 1e-6, "fuori foglio: {r:?}");
                for altra in &rettangoli[i + 1..] {
                    assert!(!sovrapposti(*r, *altra), "sovrapposti: {r:?} e {altra:?}");
                }
            }
        }
    }

    #[test]
    fn le_copie_diventano_pezzi() {
        let mut pezzi = Vec::new();
        for copia in 0..3 {
            pezzi.push(pezzo((7, 0, copia), 100.0, 60.0));
        }
        let fogli = disponi(&pezzi, 904.0, 5.0, true, ALTEZZA_MAX_MM);
        assert_eq!(fogli[0].posti.len(), 3);
        let copie: Vec<u32> = fogli[0].posti.iter().map(|p| p.id.2).collect();
        assert_eq!(copie, [0, 1, 2]);
    }

    #[test]
    fn oltre_l_altezza_massima_si_apre_un_altro_foglio() {
        // Due pezzi larghi 900 × 400, senza rotazione: ciascuno sta in una fila
        // da solo e insieme (400 + 5 + 400 = 805) superano il limite di 700.
        let pezzi = [pezzo((0, 0, 0), 900.0, 400.0), pezzo((0, 1, 0), 900.0, 400.0)];
        let fogli = disponi(&pezzi, 904.0, 5.0, false, 700.0);
        assert_eq!(fogli.len(), 2);
        assert!((fogli[0].altezza - 400.0).abs() < 1e-9);
        assert!((fogli[1].altezza - 400.0).abs() < 1e-9);
    }

    #[test]
    fn il_nuovo_foglio_riparte_dal_proprio_fondo() {
        for numero in [13, 26] {
            let pezzi: Vec<_> = (0..numero)
                .map(|n| pezzo((0, n, 0), 900.0, 400.0))
                .collect();
            let fogli = disponi(&pezzi, 904.0, 5.0, false, ALTEZZA_MAX_MM);
            assert_eq!(fogli.len(), numero.div_ceil(12));
            for (i, foglio) in fogli.iter().enumerate() {
                let attesi = (numero - i * 12).min(12);
                assert_eq!(foglio.posti.len(), attesi);
                assert_eq!(foglio.altezza, attesi as f64 * 405.0 - 5.0);
                assert_eq!(foglio.posti[0].y, 0.0);
            }
        }
    }

    /// Dopo un foglio alto, le file più basse ripartono da zero senza ereditarne il fondo.
    #[test]
    fn nuovo_foglio_con_altezze_diverse() {
        let pezzi = [
            pezzo((1, 0, 0), 900.0, 600.0),
            pezzo((2, 0, 0), 900.0, 300.0),
            pezzo((3, 0, 0), 900.0, 200.0),
        ];
        let fogli = disponi(&pezzi, 904.0, 5.0, false, 700.0);
        assert_eq!(fogli.len(), 2);
        assert_eq!(fogli[0].altezza, 600.0);
        assert_eq!(fogli[1].altezza, 505.0);
        assert_eq!(fogli[1].posti, [
            Posto { id: (2, 0, 0), x: 0.0, y: 0.0, ruotato: false },
            Posto { id: (3, 0, 0), x: 0.0, y: 305.0, ruotato: false },
        ]);
    }

    // ---- composizione ----

    fn pdf_con_pagina(l: f64, a: f64, ruota: i64) -> Vec<u8> {
        pdf_con_contenuto(l, a, ruota, b"0 0 0 rg 0 0 10 10 re f")
    }

    fn pdf_con_contenuto(l: f64, a: f64, ruota: i64, corpo: &[u8]) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let id_pagine = doc.new_object_id();
        let contenuto = doc.add_object(Stream::new(Dictionary::new(), corpo.to_vec()));
        let mut pagina = dictionary! {
            "Type" => "Page",
            "Parent" => id_pagine,
            "Contents" => contenuto,
            "Resources" => dictionary! { "ProcSet" => vec![Object::Name(b"PDF".to_vec())] },
            "MediaBox" => reali(&[0.0, 0.0, l, a]),
        };
        if ruota != 0 {
            pagina.set("Rotate", ruota);
        }
        let pagina = doc.add_object(pagina);
        doc.objects.insert(
            id_pagine,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![pagina.into()],
                "Count" => 1,
            }),
        );
        let catalogo = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => id_pagine });
        doc.trailer.set("Root", catalogo);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn leggi(dati: &[u8]) -> Document {
        Document::load_mem(dati).unwrap()
    }

    fn larghezza_altezza(doc: &Document) -> (f64, f64) {
        let id = *doc.get_pages().values().next().unwrap();
        let ([x0, y0, x1, y1], _) = file::box_e_rotazione(doc, id).unwrap();
        ((x1 - x0) / MM_PT, (y1 - y0) / MM_PT)
    }

    #[test]
    fn compone_un_foglio_con_un_pdf_e_un_immagine() {
        let pdf = pdf_con_pagina(210.0 * MM_PT, 297.0 * MM_PT, 0);
        let rgb = Contenuto::Rgb {
            larghezza: 10,
            altezza: 20,
            pixel: vec![128; 10 * 20 * 3],
        };
        let sorgenti = vec![
            Sorgente {
                id: 0,
                contenuto: Contenuto::Pdf(pdf),
                misure: vec![(210.0, 297.0)],
            },
            Sorgente { id: 1, contenuto: rgb, misure: vec![(100.0, 60.0)] },
        ];
        let pezzi = [pezzo((0, 0, 0), 210.0, 297.0), pezzo((1, 0, 0), 100.0, 60.0)];
        let fogli = disponi(&pezzi, 904.0, 5.0, false, ALTEZZA_MAX_MM);
        let dati = componi(&fogli, &sorgenti, 904.0).unwrap();
        assert_eq!(dati.len(), 1);
        let doc = leggi(&dati[0]);
        let (l, a) = larghezza_altezza(&doc);
        assert!((l - 904.0).abs() < 0.01, "{l}");
        // I due pezzi affiancati: 297 mm di altezza.
        assert!((a - 297.0).abs() < 0.01, "{a}");
        // Un Form XObject (la pagina) e un'immagine Flate.
        let tipi: Vec<String> = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .filter_map(|s| {
                let nome = s.dict.get(b"Subtype").ok()?.as_name().ok()?;
                Some(String::from_utf8_lossy(nome).into_owned())
            })
            .collect();
        assert!(tipi.iter().any(|t| t == "Form"), "{tipi:?}");
        assert!(tipi.iter().any(|t| t == "Image"), "{tipi:?}");
        let contenuto = String::from_utf8_lossy(
            &doc.get_page_content(*doc.get_pages().values().next().unwrap()),
        )
        .into_owned();
        assert!(contenuto.contains("/IQN0 Do"), "{contenuto}");
        assert!(contenuto.contains("/IQN1 Do"), "{contenuto}");
        // L'immagine ha lo spazio unitario: la matrice sono i mm del pezzo in punti.
        let riga = contenuto.lines().find(|r| r.contains("/IQN1 Do")).unwrap();
        let valori: Vec<f64> = riga
            .trim_start_matches("q ")
            .split_whitespace()
            .take(6)
            .map(|v| v.parse().unwrap())
            .collect();
        assert!((valori[0] - 100.0 * MM_PT).abs() < 0.01, "{valori:?}");
        assert!((valori[3] - 60.0 * MM_PT).abs() < 0.01, "{valori:?}");
    }

    #[test]
    fn un_pezzo_ruotato_cambia_i_lati_sul_foglio() {
        let pdf = pdf_con_pagina(210.0 * MM_PT, 297.0 * MM_PT, 0);
        let sorgenti = vec![Sorgente {
            id: 0,
            contenuto: Contenuto::Pdf(pdf),
            misure: vec![(210.0, 297.0)],
        }];
        let fogli = disponi(&[pezzo((0, 0, 0), 210.0, 297.0)], 904.0, 5.0, true, ALTEZZA_MAX_MM);
        assert!(fogli[0].posti[0].ruotato);
        let dati = componi(&fogli, &sorgenti, 904.0).unwrap();
        let doc = leggi(&dati[0]);
        let (_, a) = larghezza_altezza(&doc);
        assert!((a - 210.0).abs() < 0.01, "il pezzo sdraiato è alto 210 mm: {a}");
        let contenuto = String::from_utf8_lossy(
            &doc.get_page_content(*doc.get_pages().values().next().unwrap()),
        )
        .into_owned();
        // Rotazione di 90°: a = 0 e b negativo nella matrice `cm`.
        let matrice = contenuto.split(" cm ").next().unwrap();
        let valori: Vec<f64> = matrice
            .trim_start_matches("q ")
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        assert!(valori[0].abs() < 1e-6 && valori[1] < 0.0, "{valori:?}");
    }

    #[test]
    fn il_pdf_importato_conserva_le_risorse() {
        let pdf = pdf_con_pagina(210.0 * MM_PT, 297.0 * MM_PT, 0);
        let sorgenti = vec![Sorgente {
            id: 0,
            contenuto: Contenuto::Pdf(pdf),
            misure: vec![(210.0, 297.0)],
        }];
        let fogli = disponi(&[pezzo((0, 0, 0), 210.0, 297.0)], 904.0, 5.0, false, ALTEZZA_MAX_MM);
        let dati = componi(&fogli, &sorgenti, 904.0).unwrap();
        let doc = leggi(&dati[0]);
        let form = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .find(|s| s.dict.get(b"Subtype").and_then(Object::as_name).is_ok_and(|n| n == b"Form"))
            .expect("Form XObject");
        let risorse = form.dict.get(b"Resources").unwrap().as_dict().unwrap();
        assert!(risorse.has(b"ProcSet"), "risorse perse");
        assert!(form.dict.get(b"BBox").is_ok());
        assert!(form.dict.get(b"Matrix").is_ok());
    }

    /// BBox e Matrix del primo Form XObject del foglio.
    fn form(doc: &Document) -> (Vec<f64>, Vec<f64>) {
        let stream = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .find(|s| s.dict.get(b"Subtype").and_then(Object::as_name).is_ok_and(|n| n == b"Form"))
            .expect("Form XObject");
        let numeri = |chiave: &[u8]| -> Vec<f64> {
            stream
                .dict
                .get(chiave)
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|o| f64::from(o.as_float().unwrap()))
                .collect()
        };
        (numeri(b"BBox"), numeri(b"Matrix"))
    }

    #[test]
    fn un_pdf_ruotato_di_90_arriva_dritto() {
        // Pagina 210 × 297 con /Rotate 90: appare 297 × 210.
        let pdf = pdf_con_pagina(210.0 * MM_PT, 297.0 * MM_PT, 90);
        let sorgenti = vec![Sorgente {
            id: 0,
            contenuto: Contenuto::Pdf(pdf),
            misure: vec![(297.0, 210.0)],
        }];
        let fogli = disponi(&[pezzo((0, 0, 0), 297.0, 210.0)], 904.0, 5.0, false, ALTEZZA_MAX_MM);
        let dati = componi(&fogli, &sorgenti, 904.0).unwrap();
        let doc = leggi(&dati[0]);
        let (_, a) = larghezza_altezza(&doc);
        assert!((a - 210.0).abs() < 0.01, "{a}");
        let (bbox, matrice) = form(&doc);
        // Il BBox è quello della pagina, nello spazio del contenuto: il Matrix
        // lo porta sul box visivo e il clip si applica dopo. Con il box visivo
        // una pagina ruotata di 90/270 perdeva la parte oltre il lato corto.
        assert!((bbox[2] - 210.0 * MM_PT).abs() < 0.01, "{bbox:?}");
        assert!((bbox[3] - 297.0 * MM_PT).abs() < 0.01, "{bbox:?}");
        let punto = |x: f64, y: f64| {
            (
                matrice[0] * x + matrice[2] * y + matrice[4],
                matrice[1] * x + matrice[3] * y + matrice[5],
            )
        };
        let (x0, y0) = punto(bbox[0], bbox[1]);
        let (x1, y1) = punto(bbox[2], bbox[3]);
        assert!(((x1 - x0).abs() - 297.0 * MM_PT).abs() < 0.01, "{matrice:?}");
        assert!(((y1 - y0).abs() - 210.0 * MM_PT).abs() < 0.01, "{matrice:?}");
    }

    /// Pagina ruotata di 270 con MediaBox spostato: il BBox resta il riquadro
    /// della pagina e il Matrix lo porta sul box visivo. Con il box visivo il
    /// render tagliava via la parte oltre il lato corto (trovato con pdftoppm).
    #[test]
    fn il_box_del_form_segue_la_pagina_anche_ruotata() {
        let mut doc = Document::load_mem(&pdf_con_pagina(400.0, 300.0, 270)).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        doc.get_dictionary_mut(id)
            .unwrap()
            .set("MediaBox", reali(&[20.0, 30.0, 420.0, 330.0]));
        let mut dati = Vec::new();
        doc.save_to(&mut dati).unwrap();
        let sorgenti = vec![Sorgente {
            id: 0,
            contenuto: Contenuto::Pdf(dati),
            misure: vec![(300.0 * 25.4 / 72.0, 400.0 * 25.4 / 72.0)],
        }];
        let misure = sorgenti[0].misure[0];
        let fogli = disponi(
            &[pezzo((0, 0, 0), misure.0, misure.1)],
            904.0,
            5.0,
            false,
            ALTEZZA_MAX_MM,
        );
        let composto = componi(&fogli, &sorgenti, 904.0).unwrap();
        let doc = leggi(&composto[0]);
        let (bbox, matrice) = form(&doc);
        assert!((bbox[0] - 20.0).abs() < 1e-6, "{bbox:?}");
        assert!((bbox[1] - 30.0).abs() < 1e-6, "{bbox:?}");
        assert!((bbox[2] - 420.0).abs() < 1e-6, "{bbox:?}");
        assert!((bbox[3] - 330.0).abs() < 1e-6, "{bbox:?}");
        let punto = |x: f64, y: f64| {
            (
                matrice[0] * x + matrice[2] * y + matrice[4],
                matrice[1] * x + matrice[3] * y + matrice[5],
            )
        };
        let (x0, y0) = punto(bbox[0], bbox[1]);
        let (x1, y1) = punto(bbox[2], bbox[3]);
        assert!(((x1 - x0).abs() - 300.0).abs() < 1e-6, "{matrice:?}");
        assert!(((y1 - y0).abs() - 400.0).abs() < 1e-6, "{matrice:?}");
    }

    /// Guarda i pixel del foglio composto, reso con pdfium: i bordi della
    /// pagina ruotata ci sono tutti. A mano (pdftoppm, 5/11/2026) si è visto
    /// che con il box visivo nel BBox il clip tagliava via una striscia.
    #[test]
    fn il_render_del_foglio_conserva_i_bordi_della_pagina_ruotata() {
        use pdfium_render::prelude::PdfRenderConfig;

        let Some(pdfium) = crate::anteprima::pdfium_per_test() else { return };
        // 300 × 400 nello spazio utente e /Rotate 270: il pezzo appare 400 ×
        // 300. Bordi verdi da 20 unità su tutti i lati e quadrato rosso in
        // alto a destra nello spazio utente, che dopo la rotazione finisce in
        // alto a sinistra: dice anche che il verso è quello giusto.
        let contenuto = b"\
0 1 0 rg 0 0 300 20 re f 0 380 300 20 re f 0 0 20 400 re f 280 0 20 400 re f \
1 0 0 rg 280 380 20 20 re f";
        let dati = pdf_con_contenuto(300.0, 400.0, 270, contenuto);
        let (l, a) = (400.0 * 25.4 / 72.0, 300.0 * 25.4 / 72.0);
        let sorgenti = vec![Sorgente {
            id: 0,
            contenuto: Contenuto::Pdf(dati),
            misure: vec![(l, a)],
        }];
        let fogli = disponi(&[pezzo((0, 0, 0), l, a)], 904.0, 5.0, false, ALTEZZA_MAX_MM);
        assert!(!fogli[0].posti[0].ruotato, "senza rotazione il pezzo resta dritto");
        let composto = componi(&fogli, &sorgenti, 904.0).unwrap();
        let doc = pdfium.load_pdf_from_byte_slice(&composto[0], None).unwrap();
        let pagina = doc.pages().get(0).unwrap();
        // 2562 px per i 2562,5 pt del foglio: il pezzo viene largo 400 px e
        // alto quanto il foglio, con bordi da 20 px.
        let config = PdfRenderConfig::new().set_target_width(2562);
        let immagine = pagina.render_with_config(&config).unwrap().as_image().unwrap().to_rgba8();
        let (larghezza, altezza) = immagine.dimensions();
        assert!(larghezza >= 2500, "foglio largo {larghezza} px");
        assert!((295..=305).contains(&altezza), "foglio alto {altezza} px");
        let colore = |x: u32, y: u32| immagine.get_pixel(x, y).0;
        let verde = |x: u32, y: u32| {
            let [r, g, b, _] = colore(x, y);
            g > 150 && r < 120 && b < 120
        };
        let rosso = |x: u32, y: u32| {
            let [r, g, b, _] = colore(x, y);
            r > 150 && g < 120 && b < 120
        };
        let meta = altezza / 2;
        for (x, y) in [(8, meta), (392, meta), (200, 8), (200, altezza - 9)] {
            assert!(verde(x, y), "bordo mancante in {x},{y}: {:?}", colore(x, y));
        }
        assert!(rosso(8, 8), "angolo in alto a sinistra: {:?}", colore(8, 8));
        assert!(!rosso(392, 8), "il quadrato rosso sta solo a sinistra: {:?}", colore(392, 8));
    }

    /// JPEG creato con la crate `image`, letto senza ricompressione.
    #[test]
    fn un_jpeg_entra_come_dctdecode() {
        let percorso = FileTemporaneo::nuovo("jpg");
        image::RgbImage::from_pixel(40, 30, image::Rgb([10, 200, 10]))
            .save_with_format(percorso.percorso(), image::ImageFormat::Jpeg)
            .unwrap();
        let dati = std::fs::read(percorso.percorso()).unwrap();
        let info = dimensioni_jpeg(&dati).unwrap();
        assert_eq!((info.larghezza, info.altezza), (40, 30));
        assert_eq!(info.componenti, 3);
        assert!(!info.adobe);

        let sorgenti = vec![Sorgente {
            id: 0,
            contenuto: Contenuto::Jpeg(dati.clone()),
            misure: vec![(100.0, 75.0)],
        }];
        let fogli = disponi(&[pezzo((0, 0, 0), 100.0, 75.0)], 904.0, 5.0, false, ALTEZZA_MAX_MM);
        let composto = componi(&fogli, &sorgenti, 904.0).unwrap();
        let doc = leggi(&composto[0]);
        let immagine = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .find(|s| s.dict.get(b"Subtype").and_then(Object::as_name).is_ok_and(|n| n == b"Image"))
            .expect("immagine");
        assert_eq!(immagine.dict.get(b"Filter").unwrap().as_name().unwrap(), b"DCTDecode");
        assert_eq!(immagine.content, dati, "il JPEG non va ricompresso");
    }

    #[test]
    fn l_intestazione_jpeg_dice_se_e_adobe_cmyk() {
        // APP14 Adobe davanti a un SOF0 a 4 componenti.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xEE];
        let mut app14 = b"Adobe\0\x64\0\0\0\0".to_vec();
        jpeg.extend_from_slice(&((app14.len() + 2) as u16).to_be_bytes());
        jpeg.append(&mut app14);
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        jpeg.extend_from_slice(&90u16.to_be_bytes()); // 90 righe
        jpeg.extend_from_slice(&120u16.to_be_bytes()); // 120 colonne
        jpeg.extend_from_slice(&[4, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, 4, 0x11, 0]);
        let info = dimensioni_jpeg(&jpeg).unwrap();
        assert_eq!((info.larghezza, info.altezza), (120, 90));
        assert_eq!(info.componenti, 4);
        assert!(info.adobe);
        let immagine = immagine_jpeg(&jpeg).unwrap();
        assert_eq!(immagine.dict.get(b"ColorSpace").unwrap().as_name().unwrap(), b"DeviceCMYK");
        assert!(immagine.dict.get(b"Decode").is_ok(), "CMYK Adobe va invertito");
    }

    #[test]
    fn un_jpeg_senza_sof_da_errore() {
        assert!(dimensioni_jpeg(&[0xFF, 0xD8, 0xFF, 0xD9]).is_err());
        assert!(dimensioni_jpeg(b"non un jpeg").is_err());
    }
}
