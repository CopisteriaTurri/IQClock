//! Ritocchi che rendono un PDF digeribile dalla T2600, applicati a ogni PDF
//! prima dell'invio anche quando non c'è nessuna lavorazione.
//!
//! Provato sulla T2600 il 19/11/2026 con una scansione HP (PDFlib, linearizzata,
//! XMP con `<pdfx:HpMetadata>METADATA-</pdfx:HpMetadata>`, MediaBox
//! 812 × 531 mm e CropBox 688 × 183 mm), un job alla volta:
//! 1. così com'era: errore di pannello 0090-0007-0096 "EH-PDL parse error" in
//!    `BelenPers::configureJobSettings`;
//! 2. stessi byte con il solo elemento `HpMetadata` dell'XMP sostituito da
//!    spazi: stampato, ma grande quanto il MediaBox. La T2600 ignora il CropBox;
//! 3. riscritto con lopdf (XMP tolto, MediaBox sul CropBox, niente più
//!    linearizzazione): di nuovo 0090-0007-0096;
//! 4. come il 2 con in più il MediaBox riscritto al suo posto con le misure del
//!    CropBox: stampato perfetto.
//!
//! Per questo, quando il file partirebbe com'è, i ritocchi si fanno sui byte
//! senza cambiare lunghezza né offset ([`ritocca_byte`]): il resto del file
//! arriva alla stampante identico. Quando IQClock lo riscrive comunque (scala,
//! cornice, allineamento) si applica [`applica`] al documento lopdf, che sbianca
//! l'elemento nell'XMP invece di togliere i metadati.

use anyhow::Result;
use lopdf::xref::XrefEntry;
use lopdf::{Document, Object, ObjectId};

use crate::file;

/// Oltre questa dimensione l'XMP non si decomprime: non è un pacchetto di
/// metadati vero e lo si lascia com'è.
const XMP_MASSIMO: usize = 16 * 1024 * 1024;

/// Differenza (in unità utente) sotto la quale MediaBox e parte visibile coincidono.
const TOLLERANZA_BOX: f64 = 0.01;

/// Fin dove si cerca la fine di un elemento XMP o di un oggetto pagina.
const RICERCA_MASSIMA: usize = 64 * 1024;

const HP_METADATA: &[u8] = b"HpMetadata";

/// Ritocchi sui byte di un PDF che partirebbe com'è: sbianca gli elementi
/// `HpMetadata` negli stream XMP non compressi e riscrive al suo posto il
/// MediaBox delle pagine dove il CropBox ne lascia fuori un pezzo. Lunghezza e
/// offset restano gli stessi. `None` se non c'è niente da cambiare o il PDF non
/// si legge (o è cifrato): parte com'è.
///
/// Un XMP compresso con `HpMetadata` non si può sbiancare al suo posto: allora
/// il documento si riscrive con [`applica`]. Quel caso sulla T2600 non è
/// provato (la riscrittura della prova 3 è andata in errore, ma lì l'XMP era
/// stato tolto del tutto).
pub(crate) fn ritocca_byte(dati: &[u8]) -> Option<Vec<u8>> {
    let mut doc = Document::load_mem(dati).ok()?;
    if doc.is_encrypted() {
        return None;
    }
    if metadati(&doc).into_iter().any(|id| xmp_compresso_con_hp(&doc, id)) {
        if !applica(&mut doc).ok()? {
            return None;
        }
        let mut out = Vec::new();
        doc.save_to(&mut out).ok()?;
        return Some(out);
    }
    let mut out = dati.to_vec();
    let mut cambiato = false;
    for id in metadati(&doc) {
        if let Some(intervallo) = stream_nei_byte(&doc, id, dati) {
            cambiato |= sbianca_hp_metadata(&mut out[intervallo]);
        }
    }
    for id in doc.get_pages().into_values() {
        cambiato |= media_box_al_suo_posto(&doc, id, &mut out);
    }
    cambiato.then_some(out)
}

/// Oggetti dei metadati XMP del catalogo e delle pagine. Pagine o catalogo
/// illeggibili si saltano: il resto del PDF parte lo stesso.
fn metadati(doc: &Document) -> Vec<ObjectId> {
    let catalogo = doc.catalog().ok().into_iter();
    let pagine = doc.get_pages().into_values().filter_map(|id| doc.get_dictionary(id).ok());
    let mut ids: Vec<ObjectId> = catalogo
        .chain(pagine)
        .filter_map(|d| d.get(b"Metadata").ok()?.as_reference().ok())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn xmp_compresso_con_hp(doc: &Document, id: ObjectId) -> bool {
    let Ok(Object::Stream(stream)) = doc.get_object(id) else { return false };
    stream.dict.has(b"Filter")
        && stream
            .decompressed_content_with_limit(XMP_MASSIMO)
            .is_ok_and(|mut xmp| sbianca_hp_metadata(&mut xmp))
}

/// Dove sta nel file il contenuto dello stream non compresso `id`: solo se
/// l'oggetto non è in un object stream e i byte trovati sono proprio quelli
/// letti da lopdf.
fn stream_nei_byte(doc: &Document, id: ObjectId, dati: &[u8]) -> Option<std::ops::Range<usize>> {
    let Ok(Object::Stream(stream)) = doc.get_object(id) else { return None };
    if stream.dict.has(b"Filter") {
        return None;
    }
    let Some(XrefEntry::Normal { offset, .. }) = doc.reference_table.get(id.0) else { return None };
    let inizio = *offset as usize;
    let oggetto = dati.get(inizio..(inizio + RICERCA_MASSIMA).min(dati.len()))?;
    if !oggetto.starts_with(format!("{} {} obj", id.0, id.1).as_bytes()) {
        return None;
    }
    let mut a = inizio + trova(oggetto, b"stream")? + b"stream".len();
    if dati.get(a) == Some(&b'\r') {
        a += 1;
    }
    if dati.get(a) == Some(&b'\n') {
        a += 1;
    }
    let intervallo = a..a + stream.content.len();
    (dati.get(intervallo.clone())? == stream.content.as_slice()).then_some(intervallo)
}

/// Gli stessi ritocchi su un documento che verrà riscritto da lopdf; `true` se
/// è cambiato.
pub(crate) fn applica(doc: &mut Document) -> Result<bool> {
    let mut cambiato = sbianca_xmp(doc)?;
    for id in doc.get_pages().into_values() {
        cambiato |= media_box_sul_visibile(doc, id)?;
    }
    Ok(cambiato)
}

/// Sostituisce con spazi ogni elemento (`<pdfx:HpMetadata>…</pdfx:HpMetadata>`
/// o `<pdfx:HpMetadata/>`) o attributo (`pdfx:HpMetadata="…"`) XMP di nome
/// `HpMetadata`. La chiave `/HpMetadata` del dizionario /Info resta: nella prova
/// 2 c'era ed è stato stampato.
fn sbianca_hp_metadata(dati: &mut [u8]) -> bool {
    let mut cambiato = false;
    let mut da = 0;
    while let Some(i) = trova(&dati[da..], HP_METADATA).map(|i| i + da) {
        da = i + HP_METADATA.len();
        // Il nome finisce proprio qui (non `HpMetadataX`).
        if dati.get(da).is_some_and(|&c| carattere_nome(c) || c == b':') {
            continue;
        }
        let Some(inizio) = inizio_nome(dati, i) else { continue };
        let fine = match dati[inizio] {
            b'<' => fine_elemento(dati, inizio, da),
            _ => fine_attributo(dati, da),
        };
        // Un commento o una sezione CDATA a metà dell'intervallo: la fine trovata
        // potrebbe non essere quella vera, e sbiancare romperebbe l'XML.
        let fine = fine.filter(|&f| {
            let tratto = &dati[inizio..f];
            [&b"<!--"[..], b"-->", b"<![CDATA[", b"]]>"].iter().all(|m| trova(tratto, m).is_none())
        });
        if let Some(fine) = fine {
            dati[inizio..fine].fill(b' ');
            da = fine;
            cambiato = true;
        }
    }
    cambiato
}

fn trova(dove: &[u8], cosa: &[u8]) -> Option<usize> {
    dove.windows(cosa.len()).position(|w| w == cosa)
}

fn carattere_nome(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.')
}

/// Inizio del nome qualificato che finisce con `HpMetadata` in `i`: il `<` di un
/// tag o la prima lettera del prefisso di un attributo (preceduto da spazio).
fn inizio_nome(dati: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    if j > 0 && dati[j - 1] == b':' {
        j -= 1;
        while j > 0 && i - j < 40 && carattere_nome(dati[j - 1]) {
            j -= 1;
        }
    }
    match dati.get(j.checked_sub(1)?)? {
        b'<' => Some(j - 1),
        c if c.is_ascii_whitespace() && j < i => Some(j),
        _ => None,
    }
}

/// Fine (esclusa) dell'elemento che inizia in `inizio`; `dopo_nome` è subito
/// dopo `HpMetadata` nel tag di apertura.
fn fine_elemento(dati: &[u8], inizio: usize, dopo_nome: usize) -> Option<usize> {
    let limite = (dopo_nome + RICERCA_MASSIMA).min(dati.len());
    let chiuso = dopo_nome + trova(&dati[dopo_nome..limite], b">")?;
    if dati[chiuso - 1] == b'/' {
        return Some(chiuso + 1);
    }
    let nome = &dati[inizio + 1..dopo_nome];
    let mut chiusura = b"</".to_vec();
    chiusura.extend_from_slice(nome);
    chiusura.push(b'>');
    let fine = chiuso + trova(&dati[chiuso..limite], &chiusura)?;
    Some(fine + chiusura.len())
}

/// Fine (esclusa) del valore dell'attributo il cui nome finisce in `dopo_nome`.
fn fine_attributo(dati: &[u8], dopo_nome: usize) -> Option<usize> {
    let mut j = dopo_nome;
    while dati.get(j)?.is_ascii_whitespace() {
        j += 1;
    }
    (*dati.get(j)? == b'=').then_some(())?;
    j += 1;
    while dati.get(j)?.is_ascii_whitespace() {
        j += 1;
    }
    let virgolette = *dati.get(j)?;
    if virgolette != b'"' && virgolette != b'\'' {
        return None;
    }
    let limite = (j + 1 + RICERCA_MASSIMA).min(dati.len());
    Some(j + 1 + dati[j + 1..limite].iter().position(|&c| c == virgolette)? + 1)
}

/// Riscrive nei byte, senza cambiarne la lunghezza, il MediaBox della pagina
/// con la sua parte visibile. Solo se il MediaBox è un array scritto nella
/// pagina stessa, la pagina è un oggetto non compresso e le nuove misure ci
/// stanno nello spazio delle vecchie.
fn media_box_al_suo_posto(doc: &Document, id: ObjectId, dati: &mut [u8]) -> bool {
    let Some(proprio) = doc.get_dictionary(id).ok().and_then(|d| d.get(b"MediaBox").ok()) else {
        return false;
    };
    let (Some(media), Some((visibile, _))) = (file::riquadro(proprio), file::box_e_rotazione(doc, id)) else {
        return false;
    };
    if uguali(media, visibile) {
        return false;
    }
    let Some(XrefEntry::Normal { offset, .. }) = doc.reference_table.get(id.0) else { return false };
    let inizio = *offset as usize;
    let Some(oggetto) = dati.get(inizio..(inizio + RICERCA_MASSIMA).min(dati.len())) else { return false };
    if !oggetto.starts_with(format!("{} {} obj", id.0, id.1).as_bytes()) {
        return false;
    }
    let Some(fine) = trova(oggetto, b"endobj") else { return false };
    let Some((a, b)) = array_media_box(&oggetto[..fine], media) else { return false };
    let nuovo = format!("[{}]", visibile.map(numero).join(" "));
    let spazio = b + 1 - a;
    if nuovo.len() > spazio {
        return false;
    }
    let posto = &mut dati[inizio + a..inizio + b + 1];
    posto[..nuovo.len() - 1].copy_from_slice(&nuovo.as_bytes()[..nuovo.len() - 1]);
    posto[nuovo.len() - 1..spazio - 1].fill(b' ');
    posto[spazio - 1] = b']';
    true
}

/// Posizione di `[` e `]` dell'array MediaBox nel testo dell'oggetto pagina,
/// solo se non ci sono dubbi che sia quello che ha letto lopdf (`media`).
fn array_media_box(oggetto: &[u8], media: [f64; 4]) -> Option<(usize, usize)> {
    // Con stringhe letterali o nomi con `#` (`/Media#42ox` vale `/MediaBox`)
    // la `/MediaBox` trovata nei byte potrebbe non essere la chiave vera: in
    // una pagina sono rari, e senza non serve un vero lexer PDF.
    if oggetto.iter().any(|&c| c == b'(' || c == b'#') {
        return None;
    }
    // Una sola `/MediaBox` nell'oggetto: un'altra in un commento o in un
    // dizionario annidato renderebbe incerto quale sia quella vera.
    let chiave = trova(oggetto, b"/MediaBox")?;
    if trova(&oggetto[chiave + 1..], b"/MediaBox").is_some() {
        return None;
    }
    let mut a = chiave + b"/MediaBox".len();
    while oggetto.get(a)?.is_ascii_whitespace() {
        a += 1;
    }
    if oggetto[a] != b'[' {
        return None;
    }
    let b = a + oggetto[a..].iter().position(|&c| c == b']')?;
    // Un commento potrebbe contenere la `]` trovata: meglio non toccare niente.
    if oggetto[a..b].contains(&b'%') {
        return None;
    }
    // E l'array trovato deve dire proprio quello che ha letto lopdf.
    let letti: Vec<f64> = std::str::from_utf8(&oggetto[a + 1..b])
        .ok()?
        .split_ascii_whitespace()
        .map(|n| n.parse().ok())
        .collect::<Option<_>>()?;
    let [x0, y0, x1, y1] = <[f64; 4]>::try_from(letti).ok()?;
    uguali([x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)], media).then_some((a, b))
}

fn uguali(a: [f64; 4], b: [f64; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < TOLLERANZA_BOX)
}

/// Numero PDF corto: al massimo 4 decimali, senza zeri in coda.
fn numero(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

/// Sbianca gli elementi `HpMetadata` dell'XMP del catalogo e delle pagine.
fn sbianca_xmp(doc: &mut Document) -> Result<bool> {
    let mut cambiato = false;
    for oggetto in metadati(doc) {
        let Ok(Object::Stream(stream)) = doc.get_object_mut(oggetto) else { continue };
        let compresso = stream.dict.has(b"Filter");
        let mut xmp = if compresso {
            match stream.decompressed_content_with_limit(XMP_MASSIMO) {
                Ok(v) => v,
                Err(_) => continue,
            }
        } else {
            stream.content.clone()
        };
        if sbianca_hp_metadata(&mut xmp) {
            if compresso {
                stream.set_plain_content(xmp);
            } else {
                stream.set_content(xmp);
            }
            cambiato = true;
        }
    }
    Ok(cambiato)
}

/// Porta MediaBox e CropBox della pagina sulla parte visibile, se il CropBox
/// ne lascia fuori un pezzo. Bleed/Trim/ArtBox vanno via: potrebbero uscire
/// dalla nuova pagina e alla stampante non servono.
fn media_box_sul_visibile(doc: &mut Document, id: ObjectId) -> Result<bool> {
    let Some(media) = file::ereditato(doc, id, b"MediaBox").and_then(file::riquadro) else {
        return Ok(false);
    };
    let Some((visibile, _)) = file::box_e_rotazione(doc, id) else { return Ok(false) };
    if uguali(media, visibile) {
        return Ok(false);
    }
    let riquadro: Vec<Object> = visibile.iter().map(|&v| Object::Real(v as f32)).collect();
    let pagina = doc.get_dictionary_mut(id)?;
    pagina.set("MediaBox", riquadro.clone());
    // Il CropBox è ereditabile: va riscritto, non solo tolto.
    pagina.set("CropBox", riquadro);
    for chiave in [&b"BleedBox"[..], b"TrimBox", b"ArtBox"] {
        pagina.remove(chiave);
    }
    Ok(true)
}

#[cfg(test)]
mod test {
    use super::*;
    use lopdf::{Dictionary, Stream, dictionary};

    const XMP_HP: &[u8] = b"<x:xmpmeta><rdf:RDF><rdf:Description \
        xmlns:pdfx=\"http://ns.adobe.com/pdfx/1.3/\"><pdfx:HpMetadata>METADATA-</pdfx:HpMetadata>\
        </rdf:Description></rdf:RDF></x:xmpmeta>";

    /// PDF di una pagina con MediaBox `media`, CropBox facoltativo e XMP.
    fn pdf(media: [f64; 4], crop: Option<[f64; 4]>, xmp: Option<&[u8]>, comprimi: bool) -> Document {
        let mut doc = Document::with_version("1.6");
        let pagine = doc.new_object_id();
        let contenuto = doc.add_object(Stream::new(Dictionary::new(), b"0 0 m 10 10 l S".to_vec()));
        let a = |r: [f64; 4]| r.iter().map(|&v| Object::Real(v as f32)).collect::<Vec<_>>();
        let mut pagina = dictionary! {
            "Type" => "Page", "Parent" => pagine, "Contents" => contenuto, "MediaBox" => a(media),
            "TrimBox" => a(media),
        };
        if let Some(c) = crop {
            pagina.set("CropBox", a(c));
        }
        let pagina = doc.add_object(pagina);
        doc.objects.insert(
            pagine,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![pagina.into()], "Count" => 1 }),
        );
        let mut catalogo = dictionary! { "Type" => "Catalog", "Pages" => pagine };
        if let Some(x) = xmp {
            let mut s = Stream::new(dictionary! { "Type" => "Metadata", "Subtype" => "XML" }, x.to_vec());
            if comprimi {
                s.compress().unwrap();
                assert!(s.dict.has(b"Filter"));
            }
            catalogo.set("Metadata", doc.add_object(s));
        }
        let catalogo = doc.add_object(catalogo);
        doc.trailer.set("Root", catalogo);
        doc
    }

    fn salva(mut doc: Document) -> Vec<u8> {
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn box_pagina(doc: &Document, chiave: &[u8]) -> Option<[f64; 4]> {
        let id = *doc.get_pages().values().next().unwrap();
        doc.get_dictionary(id).unwrap().get(chiave).ok().and_then(file::riquadro)
    }

    fn circa(a: [f64; 4], b: [f64; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01)
    }

    #[test]
    fn sbianca_elementi_e_attributi() {
        let mut x = b"<a><pdfx:HpMetadata>M-</pdfx:HpMetadata><b/>\
            <HpMetadata/><rdf:D pdfx:HpMetadata='v' x=\"1\"/>/HpMetadata(M-)</a>"
            .to_vec();
        let lunghezza = x.len();
        assert!(sbianca_hp_metadata(&mut x));
        assert_eq!(x.len(), lunghezza);
        let testo = String::from_utf8(x).unwrap();
        // Resta solo la chiave del dizionario /Info, che non è XMP.
        assert_eq!(testo.matches("HpMetadata").count(), 1, "{testo}");
        assert!(testo.contains("/HpMetadata(M-)"));
        assert!(testo.contains("<b/>") && testo.contains("x=\"1\"/>"));

        // Nomi più lunghi e commenti XML in mezzo non si toccano.
        let mut x = b"<r><pdfx:HpMetadataX>a</pdfx:HpMetadataX></r>".to_vec();
        assert!(!sbianca_hp_metadata(&mut x));
        let mut x = b"<r><!-- <pdfx:HpMetadata> --><t>T</t><pdfx:HpMetadata>X</pdfx:HpMetadata></r>".to_vec();
        assert!(sbianca_hp_metadata(&mut x));
        let atteso = format!("<r><!-- <pdfx:HpMetadata> --><t>T</t>{}</r>", " ".repeat("<pdfx:HpMetadata>X</pdfx:HpMetadata>".len()));
        assert_eq!(String::from_utf8(x).unwrap(), atteso);
        let mut x = b"<pdfx:HpMetadata><![CDATA[</pdfx:HpMetadata>]]></pdfx:HpMetadata>".to_vec();
        assert!(!sbianca_hp_metadata(&mut x));
    }

    #[test]
    fn byte_come_la_prova_riuscita_sulla_t2600() {
        // Come la scansione HP: MediaBox molto più grande, CropBox in basso a
        // sinistra, XMP con HpMetadata non compresso.
        let originale = salva(pdf(
            [0.0, 0.0, 2302.56, 1506.48],
            Some([0.0, 0.0, 1950.09, 520.126]),
            Some(XMP_HP),
            false,
        ));
        let ritoccato = ritocca_byte(&originale).unwrap();
        assert_eq!(ritoccato.len(), originale.len());
        assert!(trova(&ritoccato, HP_METADATA).is_none());
        let doc = Document::load_mem(&ritoccato).unwrap();
        assert!(circa(box_pagina(&doc, b"MediaBox").unwrap(), [0.0, 0.0, 1950.09, 520.126]));
        // Il resto non cambia: l'XMP c'è ancora, e anche il TrimBox.
        assert!(doc.catalog().unwrap().has(b"Metadata"));
        assert!(box_pagina(&doc, b"TrimBox").is_some());
        // Già ritoccato: niente da fare.
        assert!(ritocca_byte(&ritoccato).is_none());
    }

    #[test]
    fn byte_senza_niente_da_fare_o_illeggibili() {
        let r = [10.0, 20.0, 605.0, 862.0];
        assert!(ritocca_byte(&salva(pdf(r, Some(r), Some(b"<x:xmpmeta/>"), false))).is_none());
        assert!(ritocca_byte(b"non e un pdf").is_none());
    }

    #[test]
    fn media_box_che_non_sta_nel_suo_spazio_resta() {
        // "[0 0 100 100]" non può contenere le misure del CropBox.
        let originale = salva(pdf([0.0, 0.0, 100.0, 100.0], Some([0.5, 0.25, 99.75, 99.125]), None, false));
        assert!(ritocca_byte(&originale).is_none());
    }

    #[test]
    fn byte_il_testo_delle_pagine_non_si_tocca() {
        // Un contenuto di pagina non compresso che "dice" HpMetadata resta com'è.
        let mut doc = pdf([0.0, 0.0, 595.0, 842.0], None, None, false);
        let pagina = *doc.get_pages().values().next().unwrap();
        let contenuto = doc.get_dictionary(pagina).unwrap().get(b"Contents").unwrap().as_reference().unwrap();
        let testo = b"BT /F1 12 Tf (<pdfx:HpMetadata>x</pdfx:HpMetadata>) Tj ET".to_vec();
        doc.get_object_mut(contenuto).unwrap().as_stream_mut().unwrap().set_content(testo);
        assert!(ritocca_byte(&salva(doc)).is_none());
    }

    #[test]
    fn byte_xmp_compresso_si_riscrive_sbiancato() {
        let originale = salva(pdf([0.0, 0.0, 595.0, 842.0], None, Some(XMP_HP), true));
        let ritoccato = ritocca_byte(&originale).unwrap();
        let doc = Document::load_mem(&ritoccato).unwrap();
        let id = doc.catalog().unwrap().get(b"Metadata").unwrap().as_reference().unwrap();
        let xmp = doc.get_object(id).unwrap().as_stream().unwrap().get_plain_content().unwrap();
        assert!(trova(&xmp, HP_METADATA).is_none());
        assert!(trova(&xmp, b"rdf:Description").is_some());
    }

    #[test]
    fn byte_xmp_compresso_senza_elementi_da_togliere_resta() {
        // Abbastanza lungo perché lopdf lo comprima davvero.
        let xmp = [&b"<rdf:Description pdfx:HpMetadataX=\"v\"/>"[..], &[b' '; 200]].concat();
        let originale = salva(pdf([0.0, 0.0, 595.0, 842.0], None, Some(&xmp), true));
        assert!(ritocca_byte(&originale).is_none());
    }

    #[test]
    fn array_media_box_solo_senza_dubbi() {
        let m = [0.0, 0.0, 900.0, 900.0];
        let semplice = b"17 0 obj\n<</Type/Page/MediaBox [0 0 900 900]/Parent 2 0 R>>\n";
        assert_eq!(array_media_box(semplice, m), Some((31, 43)));
        for dubbio in [
            // La chiave vera è `/Media#42ox`, la stringa ne contiene una finta.
            &b"<</Media#42ox[0 0 1 1]/P(/MediaBox[0 0 900 900])>>"[..],
            // Due `/MediaBox`, una in un dizionario annidato.
            b"<</MediaBox[0 0 900 900]/X<</MediaBox[0 0 900 900]>>>>",
            // Numeri diversi da quelli letti da lopdf.
            b"<</MediaBox[0 0 900 901]>>",
            // Commento con dentro la `]`.
            b"<</MediaBox[0 0 %]\n900 900]>>",
            // Non è un array.
            b"<</MediaBox 5 0 R>>",
        ] {
            assert_eq!(array_media_box(dubbio, m), None, "{}", String::from_utf8_lossy(dubbio));
        }
    }

    #[test]
    fn byte_media_box_con_commento_resta() {
        // `[0 0 2302.56 %]` + a capo + `1506 ]`: per lopdf vale [0 0 2302.56 1506],
        // ma la prima `]` è nel commento.
        let originale = salva(pdf([0.0, 0.0, 2302.56, 1506.48], Some([0.0, 0.0, 1950.09, 520.126]), None, false));
        let vecchio = b"/MediaBox[0 0 2302.56 1506.48]";
        let i = trova(&originale, vecchio).expect("MediaBox scritto come atteso");
        let mut nuovo = b"/MediaBox[0 0 2302.56 %]\n1506".to_vec();
        nuovo.resize(vecchio.len() - 1, b' ');
        nuovo.push(b']');
        let mut dati = originale.clone();
        dati[i..i + vecchio.len()].copy_from_slice(&nuovo);
        let doc = Document::load_mem(&dati).unwrap();
        assert!(circa(box_pagina(&doc, b"MediaBox").unwrap(), [0.0, 0.0, 2302.56, 1506.0]));
        assert!(ritocca_byte(&dati).is_none());
    }

    #[test]
    fn documento_xmp_sbiancato_anche_compresso() {
        for comprimi in [false, true] {
            let mut doc = pdf([0.0, 0.0, 595.0, 842.0], None, Some(XMP_HP), comprimi);
            assert!(applica(&mut doc).unwrap());
            assert!(doc.catalog().unwrap().has(b"Metadata"));
            assert!(trova(&salva(doc), HP_METADATA).is_none(), "l'elemento è rimasto nel file");
        }
    }

    #[test]
    fn documento_media_box_portato_sul_crop_box() {
        let mut doc = pdf([0.0, 0.0, 2302.56, 1506.48], Some([0.0, 0.0, 1950.09, 520.126]), None, false);
        assert!(applica(&mut doc).unwrap());
        for chiave in [&b"MediaBox"[..], b"CropBox"] {
            assert!(circa(box_pagina(&doc, chiave).unwrap(), [0.0, 0.0, 1950.09, 520.126]));
        }
        assert!(box_pagina(&doc, b"TrimBox").is_none());
    }

    #[test]
    fn documento_senza_niente_da_fare() {
        let r = [10.0, 20.0, 605.0, 862.0];
        let mut doc = pdf(r, Some(r), Some(b"<x:xmpmeta/>"), false);
        assert!(!applica(&mut doc).unwrap());
        assert!(box_pagina(&doc, b"TrimBox").is_some());
    }
}
