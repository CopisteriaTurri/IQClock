//! Client SNMP v1 minimo (GET e GETNEXT) per leggere lo stato della stampante.
//!
//! La T2600 a `@PJL INFO STATUS` risponde solo `"?"`: lo stato vero si legge
//! dalle MIB standard (HOST-RESOURCES-MIB, Printer-MIB RFC 3805), come fa HP Click.

use std::net::{ToSocketAddrs, UdpSocket};
use std::time::Duration;

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug, PartialEq)]
pub enum Valore {
    Intero(i64),
    Testo(Vec<u8>),
    Oid(Vec<u32>),
    Nullo,
    Altro(u8, Vec<u8>),
}

impl Valore {
    pub fn intero(&self) -> Option<i64> {
        match self {
            Valore::Intero(i) => Some(*i),
            _ => None,
        }
    }

    pub fn testo(&self) -> Option<String> {
        match self {
            Valore::Testo(b) => Some(String::from_utf8_lossy(b).trim_end_matches('\0').trim().to_string()),
            _ => None,
        }
    }
}

pub fn oid(s: &str) -> Vec<u32> {
    s.trim_start_matches('.').split('.').filter_map(|p| p.parse().ok()).collect()
}

// --- Codifica BER ---------------------------------------------------------

fn tlv(tag: u8, contenuto: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = contenuto.len();
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let byte: Vec<u8> = n.to_be_bytes().into_iter().skip_while(|&b| b == 0).collect();
        out.push(0x80 | byte.len() as u8);
        out.extend(byte);
    }
    out.extend_from_slice(contenuto);
    out
}

fn intero(v: i64) -> Vec<u8> {
    let b = v.to_be_bytes();
    // Toglie i byte iniziali ridondanti mantenendo il segno.
    let mut i = 0;
    while i < 7 && ((b[i] == 0 && b[i + 1] & 0x80 == 0) || (b[i] == 0xFF && b[i + 1] & 0x80 != 0)) {
        i += 1;
    }
    tlv(0x02, &b[i..])
}

fn codifica_oid(oid: &[u32]) -> Vec<u8> {
    let mut c = vec![(oid[0] * 40 + oid[1]) as u8];
    for &arco in &oid[2..] {
        let mut gruppi = vec![(arco & 0x7F) as u8];
        let mut v = arco >> 7;
        while v > 0 {
            gruppi.push((v & 0x7F) as u8 | 0x80);
            v >>= 7;
        }
        c.extend(gruppi.into_iter().rev());
    }
    tlv(0x06, &c)
}

fn richiesta(comunita: &str, id: i32, getnext: bool, oids: &[Vec<u32>]) -> Vec<u8> {
    let varbind: Vec<u8> = oids
        .iter()
        .flat_map(|o| tlv(0x30, &[codifica_oid(o), vec![0x05, 0x00]].concat()))
        .collect();
    let pdu = [intero(id.into()), intero(0), intero(0), tlv(0x30, &varbind)].concat();
    let pdu = tlv(if getnext { 0xA1 } else { 0xA0 }, &pdu);
    tlv(0x30, &[intero(0), tlv(0x04, comunita.as_bytes()), pdu].concat())
}

// --- Decodifica BER -------------------------------------------------------

struct Lettore<'a>(&'a [u8]);

impl<'a> Lettore<'a> {
    fn leggi(&mut self) -> Result<(u8, &'a [u8])> {
        let d = self.0;
        if d.len() < 2 {
            bail!("risposta SNMP troncata");
        }
        let tag = d[0];
        let (lung, inizio) = if d[1] & 0x80 == 0 {
            (d[1] as usize, 2)
        } else {
            let n = (d[1] & 0x7F) as usize;
            if n == 0 || n > 4 || d.len() < 2 + n {
                bail!("lunghezza BER non valida");
            }
            (d[2..2 + n].iter().fold(0usize, |a, &b| a << 8 | b as usize), 2 + n)
        };
        if d.len() < inizio + lung {
            bail!("risposta SNMP troncata");
        }
        self.0 = &d[inizio + lung..];
        Ok((tag, &d[inizio..inizio + lung]))
    }

    fn atteso(&mut self, tag: u8) -> Result<&'a [u8]> {
        let (t, c) = self.leggi()?;
        if t != tag {
            bail!("tag BER inatteso: {t:#04x} invece di {tag:#04x}");
        }
        Ok(c)
    }
}

fn decodifica_intero(c: &[u8]) -> i64 {
    let iniziale = if c.first().is_some_and(|b| b & 0x80 != 0) { -1 } else { 0 };
    c.iter().fold(iniziale, |a, &b| a << 8 | b as i64)
}

fn decodifica_oid(c: &[u8]) -> Vec<u32> {
    let Some((&primo, resto)) = c.split_first() else { return Vec::new() };
    let mut out = vec![(primo / 40) as u32, (primo % 40) as u32];
    let mut v = 0u32;
    for &b in resto {
        v = v << 7 | (b & 0x7F) as u32;
        if b & 0x80 == 0 {
            out.push(v);
            v = 0;
        }
    }
    out
}

fn decodifica_risposta(dati: &[u8], id_atteso: i32) -> Result<Vec<(Vec<u32>, Valore)>> {
    let mut msg = Lettore(Lettore(dati).atteso(0x30)?);
    msg.atteso(0x02)?; // versione
    msg.atteso(0x04)?; // comunità
    let mut pdu = Lettore(msg.atteso(0xA2)?);
    if decodifica_intero(pdu.atteso(0x02)?) != id_atteso as i64 {
        bail!("risposta SNMP a un'altra richiesta");
    }
    let errore = decodifica_intero(pdu.atteso(0x02)?);
    pdu.atteso(0x02)?;
    let mut lista = Lettore(pdu.atteso(0x30)?);
    let mut out = Vec::new();
    // noSuchName (2) in v1 vuol dire "fine della tabella" per GETNEXT: lista vuota.
    if errore == 2 {
        return Ok(out);
    }
    if errore != 0 {
        bail!("errore SNMP {errore}");
    }
    while !lista.0.is_empty() {
        let mut vb = Lettore(lista.atteso(0x30)?);
        let o = decodifica_oid(vb.atteso(0x06)?);
        let (tag, c) = vb.leggi()?;
        let v = match tag {
            0x02 | 0x41 | 0x42 | 0x43 => Valore::Intero(decodifica_intero(c)),
            0x04 => Valore::Testo(c.to_vec()),
            0x06 => Valore::Oid(decodifica_oid(c)),
            0x05 => Valore::Nullo,
            t => Valore::Altro(t, c.to_vec()),
        };
        out.push((o, v));
    }
    Ok(out)
}

// --- Trasporto ------------------------------------------------------------

pub struct Client {
    sock: UdpSocket,
    comunita: String,
    prossimo_id: i32,
}

impl Client {
    pub fn new(host: &str, comunita: &str) -> Result<Self> {
        Self::new_con_porta(host, 161, comunita)
    }

    pub fn new_con_porta(host: &str, porta: u16, comunita: &str) -> Result<Self> {
        let indirizzo = (host, porta)
            .to_socket_addrs()?
            .next()
            .with_context(|| format!("nessun indirizzo per {host}"))?;
        let sock = UdpSocket::bind(if indirizzo.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
        sock.connect(indirizzo)?;
        sock.set_read_timeout(Some(Duration::from_secs(2)))?;
        Ok(Self { sock, comunita: comunita.into(), prossimo_id: std::process::id() as i32 & 0x7FFF })
    }

    fn scambia(&mut self, getnext: bool, oids: &[Vec<u32>]) -> Result<Vec<(Vec<u32>, Valore)>> {
        self.prossimo_id = self.prossimo_id.wrapping_add(1) & 0x7FFF_FFFF;
        let id = self.prossimo_id;
        let pacchetto = richiesta(&self.comunita, id, getnext, oids);
        let mut buf = vec![0u8; 65535];
        for _ in 0..2 {
            self.sock.send(&pacchetto)?;
            while let Ok(n) = self.sock.recv(&mut buf) {
                // Una risposta in ritardo a una richiesta precedente non si decodifica: si scarta.
                if let Ok(r) = decodifica_risposta(&buf[..n], id) {
                    return Ok(r);
                }
            }
        }
        bail!("nessuna risposta SNMP (stampante spenta o SNMP disattivato?)")
    }

    pub fn get(&mut self, oids: &[Vec<u32>]) -> Result<Vec<(Vec<u32>, Valore)>> {
        self.scambia(false, oids)
    }

    /// Tutte le voci sotto `base`, con GETNEXT successivi.
    pub fn walk(&mut self, base: &[u32]) -> Result<Vec<(Vec<u32>, Valore)>> {
        let mut out = Vec::new();
        let mut corrente = base.to_vec();
        while out.len() < 500 {
            let Some((o, v)) = self.scambia(true, std::slice::from_ref(&corrente))?.into_iter().next() else {
                break;
            };
            if !o.starts_with(base) || o <= corrente {
                break;
            }
            corrente = o.clone();
            out.push((o, v));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::net::SocketAddr;

    // --- BER scritto a mano: i test decodificano byte indipendenti dal
    // codificatore sotto test. ---

    fn ber(tag: u8, contenuto: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let n = contenuto.len();
        if n < 0x80 {
            out.push(n as u8);
        } else {
            let byte: Vec<u8> = n.to_be_bytes().into_iter().skip_while(|&b| b == 0).collect();
            out.push(0x80 | byte.len() as u8);
            out.extend(byte);
        }
        out.extend_from_slice(contenuto);
        out
    }

    fn ber_intero(v: i64) -> Vec<u8> {
        let b = v.to_be_bytes();
        let mut i = 0;
        while i < 7 && ((b[i] == 0 && b[i + 1] & 0x80 == 0) || (b[i] == 0xFF && b[i + 1] & 0x80 != 0)) {
            i += 1;
        }
        ber(0x02, &b[i..])
    }

    fn oid_contenuto(o: &[u32]) -> Vec<u8> {
        let mut c = vec![(o[0] * 40 + o[1]) as u8];
        for &arco in &o[2..] {
            let mut gruppi = vec![(arco & 0x7F) as u8];
            let mut v = arco >> 7;
            while v > 0 {
                gruppi.push((v & 0x7F) as u8 | 0x80);
                v >>= 7;
            }
            c.extend(gruppi.into_iter().rev());
        }
        c
    }

    fn ber_oid(o: &[u32]) -> Vec<u8> {
        ber(0x06, &oid_contenuto(o))
    }

    /// GetResponse SNMPv1 completa, pronto da mandare sulla socket.
    fn risposta(id: i32, errore: i32, varbind: &[(Vec<u32>, Vec<u8>)]) -> Vec<u8> {
        let lista: Vec<u8> = varbind
            .iter()
            .map(|(o, v)| ber(0x30, &[ber_oid(o), v.clone()].concat()))
            .collect::<Vec<Vec<u8>>>()
            .concat();
        let pdu = ber(
            0xA2,
            &[ber_intero(id.into()), ber_intero(errore.into()), ber_intero(0), ber(0x30, &lista)]
                .concat(),
        );
        ber(0x30, &[ber_intero(0), ber(0x04, b"public"), pdu].concat())
    }

    fn id_richiesta(pacchetto: &[u8]) -> i32 {
        let mut msg = Lettore(Lettore(pacchetto).atteso(0x30).unwrap());
        msg.atteso(0x02).unwrap();
        msg.atteso(0x04).unwrap();
        let mut pdu = Lettore(msg.leggi().unwrap().1);
        decodifica_intero(pdu.atteso(0x02).unwrap()) as i32
    }

    fn client_verso(indirizzo: SocketAddr) -> Client {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.connect(indirizzo).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        Client { sock, comunita: "public".into(), prossimo_id: 0 }
    }

    // --- codifica ---

    #[test]
    fn oid_andata_e_ritorno() {
        let o = oid("1.3.6.1.2.1.43.11.1.1.9.1.300");
        let c = codifica_oid(&o);
        assert_eq!(decodifica_oid(&c[2..]), o);
        // Il punto iniziale è ammesso.
        assert_eq!(oid(".1.3.6.1.2.1.1.1.0"), oid("1.3.6.1.2.1.1.1.0"));
    }

    #[test]
    fn oid_con_archi_multibyte() {
        assert_eq!(
            codifica_oid(&oid("1.3.6.1.2.1.1.1.0"))[2..].to_vec(),
            vec![0x2B, 0x06, 0x01, 0x02, 0x01, 0x01, 0x01, 0x00]
        );
        // 128 = 0x81 0x00, 300 = 0x82 0x2C, 16384 = 0x81 0x80 0x00.
        for (arco, atteso) in [
            (128u32, vec![0x81u8, 0x00]),
            (300, vec![0x82, 0x2C]),
            (16384, vec![0x81, 0x80, 0x00]),
        ] {
            let c = codifica_oid(&oid("1.3.6.1.4.1.11.2.3.9.4.2.1.4.1.5.4.1").into_iter().chain([arco]).collect::<Vec<_>>());
            assert_eq!(c[c.len() - atteso.len()..].to_vec(), atteso, "{arco}");
        }
        assert_eq!(decodifica_oid(&[]), Vec::<u32>::new());
    }

    #[test]
    fn interi_con_segno() {
        for v in [0, 1, 127, 128, 255, 256, 65535, -1, -2, -129, -32768, 1 << 40, i64::MAX, i64::MIN] {
            let c = intero(v);
            assert_eq!(decodifica_intero(&c[2..]), v, "{v}");
        }
        assert_eq!(intero(128), vec![0x02, 0x02, 0x00, 0x80]);
        assert_eq!(intero(-2), vec![0x02, 0x01, 0xFE]);
    }

    #[test]
    fn lunghezze_ber_corte_e_lunghe() {
        for (n, prefisso) in [
            (0, vec![0x04, 0x00]),
            (1, vec![0x04, 0x01]),
            (127, vec![0x04, 0x7F]),
            (128, vec![0x04, 0x81, 0x80]),
            (300, vec![0x04, 0x82, 0x01, 0x2C]),
            (70_000, vec![0x04, 0x83, 0x01, 0x11, 0x70]),
        ] {
            let c = tlv(0x04, &vec![0xAA; n]);
            assert_eq!(c[..prefisso.len()].to_vec(), prefisso, "{n}");
            assert_eq!(c.len(), prefisso.len() + n, "{n}");
        }
    }

    #[test]
    fn richiesta_get_come_net_snmp() {
        // snmpget -v1 -c public HOST 1.3.6.1.2.1.1.1.0 con request-id 1
        let atteso = [
            0x30, 0x26, 0x02, 0x01, 0x00, 0x04, 0x06, b'p', b'u', b'b', b'l', b'i', b'c', 0xA0, 0x19,
            0x02, 0x01, 0x01, 0x02, 0x01, 0x00, 0x02, 0x01, 0x00, 0x30, 0x0E, 0x30, 0x0C, 0x06, 0x08,
            0x2B, 0x06, 0x01, 0x02, 0x01, 0x01, 0x01, 0x00, 0x05, 0x00,
        ];
        assert_eq!(richiesta("public", 1, false, &[oid("1.3.6.1.2.1.1.1.0")]), atteso);
    }

    #[test]
    fn richiesta_getnext() {
        // Stessa richiesta GET ma con PDU GetNextRequest (0xA1).
        let mut atteso = richiesta("public", 1, false, &[oid("1.3.6.1.2.1.1.1.0")]);
        atteso[13] = 0xA1;
        assert_eq!(richiesta("public", 1, true, &[oid("1.3.6.1.2.1.1.1.0")]), atteso);
    }

    #[test]
    fn richiesta_con_comunita_lunga() {
        let r = richiesta(&"c".repeat(200), 1, false, &[oid("1.3.6.1.2.1.1.1.0")]);
        // Lunghezza in forma lunga: 0x81 0xE9, comunità 0x81 0xC8.
        assert_eq!(r[..9].to_vec(), vec![0x30, 0x81, 0xE9, 0x02, 0x01, 0x00, 0x04, 0x81, 0xC8]);
        assert_eq!(r.len(), 236);
    }

    // --- decodifica ---

    #[test]
    fn decodifica_get_response_hr_printer_status() {
        // Scritta a mano (non è una cattura dalla T2600): GetResponse per
        // hrPrinterStatus con request-id 7, nessun errore, INTEGER 3 (idle).
        let pacchetto = [
            0x30, 0x2A, 0x02, 0x01, 0x00, 0x04, 0x06, b'p', b'u', b'b', b'l', b'i', b'c', 0xA2, 0x1D,
            0x02, 0x01, 0x07, 0x02, 0x01, 0x00, 0x02, 0x01, 0x00, 0x30, 0x12, 0x30, 0x10, 0x06, 0x0B,
            0x2B, 0x06, 0x01, 0x02, 0x01, 0x19, 0x03, 0x05, 0x01, 0x01, 0x01, 0x02, 0x01, 0x03,
        ];
        assert_eq!(
            decodifica_risposta(&pacchetto, 7).unwrap(),
            vec![(oid("1.3.6.1.2.1.25.3.5.1.1.1"), Valore::Intero(3))]
        );
    }

    #[test]
    fn decodifica_tutti_i_tipi_di_valore() {
        let r = risposta(
            7,
            0,
            &[
                (oid("1.3.6.1.2.1.1.1.0"), ber(0x04, b"HP DesignJet T2600\0")),
                (oid("1.3.6.1.2.1.25.3.5.1.1.1"), ber_intero(-3)),
                (oid("1.3.6.1.2.1.1.2.0"), ber(0x06, &oid_contenuto(&oid("1.3.6.1.4.1.311")))),
                (oid("1.3.6.1.2.1.1.3.0"), ber(0x05, &[])),
                (oid("1.3.6.1.2.1.4.20.1.1"), ber(0x40, &[192, 168, 10, 72])),
                (oid("1.3.6.1.2.1.43.10.2.1.4.1.1"), ber(0x41, &[0x00, 0x00, 0x01, 0x2C])),
                (oid("1.3.6.1.2.1.43.11.1.1.9.1.1"), ber(0x42, &[0x00, 0x50])),
                (oid("1.3.6.1.2.1.1.3.0"), ber(0x43, &[0x00, 0x01])),
            ],
        );
        assert_eq!(
            decodifica_risposta(&r, 7).unwrap(),
            vec![
                (oid("1.3.6.1.2.1.1.1.0"), Valore::Testo(b"HP DesignJet T2600\0".to_vec())),
                (oid("1.3.6.1.2.1.25.3.5.1.1.1"), Valore::Intero(-3)),
                (oid("1.3.6.1.2.1.1.2.0"), Valore::Oid(oid("1.3.6.1.4.1.311"))),
                (oid("1.3.6.1.2.1.1.3.0"), Valore::Nullo),
                (oid("1.3.6.1.2.1.4.20.1.1"), Valore::Altro(0x40, vec![192, 168, 10, 72])),
                (oid("1.3.6.1.2.1.43.10.2.1.4.1.1"), Valore::Intero(300)),
                (oid("1.3.6.1.2.1.43.11.1.1.9.1.1"), Valore::Intero(80)),
                (oid("1.3.6.1.2.1.1.3.0"), Valore::Intero(1)),
            ]
        );
    }

    #[test]
    fn testo_toglie_nul_e_spazi_finali() {
        let v = Valore::Testo(b"HP 730 130ML ( P2V63A ) \0".to_vec());
        assert_eq!(v.testo().unwrap(), "HP 730 130ML ( P2V63A )");
        assert_eq!(Valore::Intero(1).testo(), None);
        assert_eq!(Valore::Intero(1).intero(), Some(1));
        assert_eq!(Valore::Nullo.intero(), None);
    }

    #[test]
    fn decodifica_una_risposta_con_lunghezza_lunga() {
        let nome = vec![b'A'; 300];
        let r = risposta(9, 0, &[(oid("1.3.6.1.2.1.43.11.1.1.6.1.1"), ber(0x04, &nome))]);
        let voci = decodifica_risposta(&r, 9).unwrap();
        assert_eq!(voci.len(), 1);
        assert_eq!(voci[0].1, Valore::Testo(nome));
    }

    #[test]
    fn rifiuta_risposte_di_altre_richieste_o_con_errori() {
        let equivalgono = |r: Vec<u8>, id| decodifica_risposta(&r, id).unwrap_err().to_string();

        let err = equivalgono(risposta(8, 0, &[]), 7);
        assert!(err.contains("altra richiesta"), "{err}");

        // noSuchName (2) in v1 = fine tabella, non un errore.
        assert_eq!(decodifica_risposta(&risposta(7, 2, &[]), 7).unwrap(), Vec::new());

        let err = equivalgono(risposta(7, 5, &[]), 7);
        assert!(err.contains("errore SNMP 5"), "{err}");
    }

    #[test]
    fn rifiuta_pacchetti_troncati_o_malformati() {
        for (dati, id) in [
            (vec![], 1i32),
            (vec![0x30], 1),
            (vec![0x30, 0x05, 0x02, 0x01], 1),
            (vec![0x31, 0x00], 1),
            (vec![0x30, 0x85, 0x00, 0x00, 0x00, 0x00, 0x00], 1),
            (vec![0x30, 0x03, 0x02, 0x01, 0x00], 1),
        ] {
            let r = std::panic::catch_unwind(|| decodifica_risposta(&dati, id));
            assert!(r.is_ok(), "panic su {dati:?}");
            assert!(r.unwrap().is_err(), "accettato {dati:?}");
        }
        let err = decodifica_risposta(&[0x30, 0x85, 0, 0, 0, 0, 0], 1).unwrap_err().to_string();
        assert!(err.contains("lunghezza BER"), "{err}");
        let err = decodifica_risposta(&[0x30, 0x05, 0x02], 1).unwrap_err().to_string();
        assert!(err.contains("troncata"), "{err}");
    }

    // --- trasporto: finto agente SNMP su UDP ---

    #[test]
    fn get_scarta_le_risposte_di_altre_richieste() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let indirizzo = server.local_addr().unwrap();
        let stato = oid("1.3.6.1.2.1.25.3.5.1.1.1");
        let varbind = (stato.clone(), ber_intero(3));
        let atteso = (stato.clone(), Valore::Intero(3));
        let per_l_agente = varbind;
        let agente = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).unwrap();
            let id = id_richiesta(&buf[..n]);
            // Prima la risposta (in ritardo) di un'altra richiesta, poi quella giusta.
            server
                .send_to(&risposta(id + 1, 0, std::slice::from_ref(&per_l_agente)), peer)
                .unwrap();
            server.send_to(&risposta(id, 0, std::slice::from_ref(&per_l_agente)), peer).unwrap();
        });

        let mut client = client_verso(indirizzo);
        assert_eq!(client.get(std::slice::from_ref(&stato)).unwrap(), vec![atteso]);
        agente.join().unwrap();
    }

    #[test]
    fn walk_si_ferma_a_fine_tabella() {
        let base = oid("1.3.6.1.2.1.43.11.1.1.9.1");
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let indirizzo = server.local_addr().unwrap();
        let base_agente = base.clone();
        let agente = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            for i in 1..=2u32 {
                let (n, peer) = server.recv_from(&mut buf).unwrap();
                let id = id_richiesta(&buf[..n]);
                let mut o = base_agente.clone();
                o.push(i);
                let r = risposta(id, 0, &[(o, ber_intero(10 + i as i64))]);
                server.send_to(&r, peer).unwrap();
            }
            // noSuchName: in SNMPv1 è la fine della tabella.
            let (n, peer) = server.recv_from(&mut buf).unwrap();
            let id = id_richiesta(&buf[..n]);
            server.send_to(&risposta(id, 2, &[]), peer).unwrap();
        });

        let mut client = client_verso(indirizzo);
        let voci = client.walk(&base).unwrap();
        assert_eq!(voci.len(), 2);
        assert_eq!(voci[0].1, Valore::Intero(11));
        assert_eq!(voci[1].1, Valore::Intero(12));
        let mut atteso = base.clone();
        atteso.push(2);
        assert_eq!(voci[1].0, atteso);
        agente.join().unwrap();
    }

    #[test]
    fn walk_si_ferma_su_end_of_mib_view_e_no_such_object() {
        for tag in [0x80u8, 0x82] {
            let base = oid("1.3.6.1.2.1.43.11.1.1.9.1");
            let server = UdpSocket::bind("127.0.0.1:0").unwrap();
            let indirizzo = server.local_addr().unwrap();
            let base_agente = base.clone();
            let agente = std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let (n, peer) = server.recv_from(&mut buf).unwrap();
                let id = id_richiesta(&buf[..n]);
                // Eccezione SNMPv2 sullo stesso OID richiesto.
                server.send_to(&risposta(id, 0, &[(base_agente, ber(tag, &[]))]), peer).unwrap();
            });

            let mut client = client_verso(indirizzo);
            assert!(client.walk(&base).unwrap().is_empty(), "tag {tag:#04x}");
            agente.join().unwrap();
        }
    }

    #[test]
    fn walk_si_ferma_fuori_dal_sottoalbero() {
        let base = oid("1.3.6.1.2.1.43.11.1.1.9.1");
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let indirizzo = server.local_addr().unwrap();
        let agente = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).unwrap();
            let id = id_richiesta(&buf[..n]);
            let altro = oid("1.3.6.1.2.1.1.1.0");
            server.send_to(&risposta(id, 0, &[(altro, ber(0x04, b"x"))]), peer).unwrap();
        });

        let mut client = client_verso(indirizzo);
        assert!(client.walk(&base).unwrap().is_empty());
        agente.join().unwrap();
    }
}
