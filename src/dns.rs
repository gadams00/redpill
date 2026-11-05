//! Tiny DNS responder for the captive portal: every name resolves to the board's own address,
//! so a phone that joined the setup network lands on the portal whatever it tries to open.
//! Only packet parsing and building live here (pure, tested on the host); the UDP socket loop is
//! in `main.rs`.

/// TTL on the answers. Short, so the real address is looked up again once the phone is back on
/// the home network.
const TTL_SECS: u32 = 30;
const TYPE_A: u16 = 1;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;

/// Build the response to a DNS query, or `None` if the packet is not a plain query we can answer.
///
/// * `A` (and `ANY`) questions are answered with `ip`.
/// * Other question types (AAAA, HTTPS, ...) get an empty, successful answer, so clients stop
///   waiting and use the `A` record.
pub fn answer(query: &[u8], ip: [u8; 4]) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([query[2], query[3]]);
    let is_query = flags & 0x8000 == 0;
    let opcode = (flags >> 11) & 0xF;
    let questions = u16::from_be_bytes([query[4], query[5]]);
    if !is_query || opcode != 0 || questions == 0 {
        return None;
    }

    // First question: a name made of length-prefixed labels, then type and class.
    let mut pos = 12;
    loop {
        let len = *query.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        // Compression pointers are not valid in a question; also bounds the name length.
        if len & 0xC0 != 0 || pos - 12 > 255 {
            return None;
        }
        pos += 1 + len;
    }
    let qtype = u16::from_be_bytes([*query.get(pos)?, *query.get(pos + 1)?]);
    let qclass = u16::from_be_bytes([*query.get(pos + 2)?, *query.get(pos + 3)?]);
    let question_end = pos + 4;

    let wants_a = (qtype == TYPE_A || qtype == TYPE_ANY) && qclass == CLASS_IN;

    let mut out = Vec::with_capacity(question_end + 16);
    out.extend_from_slice(&query[0..2]); // id
    // Response, authoritative, recursion desired (copied) and available; no error.
    let rd = flags & 0x0100;
    out.extend_from_slice(&(0x8000u16 | 0x0400 | rd | 0x0080).to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // one question (we echo only the first)
    out.extend_from_slice(&u16::from(wants_a).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]); // no authority or additional records
    out.extend_from_slice(&query[12..question_end]);
    if wants_a {
        out.extend_from_slice(&[0xC0, 0x0C]); // name: pointer to the question
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&TTL_SECS.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&ip);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: [u8; 4] = [192, 168, 4, 1];

    /// A standard query like a phone sends: id 0x1234, recursion desired, one question.
    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    #[test]
    fn a_query_is_answered_with_the_board_address() {
        let q = query("connectivitycheck.gstatic.com", 1);
        let r = answer(&q, IP).unwrap();
        assert_eq!(&r[0..2], &[0x12, 0x34], "same id");
        assert_eq!(r[2] & 0x80, 0x80, "marked as a response");
        assert_eq!(r[3] & 0x0F, 0, "no error");
        assert_eq!(&r[4..6], &[0, 1], "one question");
        assert_eq!(&r[6..8], &[0, 1], "one answer");
        assert_eq!(&r[12..q.len()], &q[12..], "question echoed");
        assert_eq!(&r[r.len() - 4..], &IP, "answer is the board address");
        assert_eq!(&r[q.len()..q.len() + 2], &[0xC0, 0x0C]);
        assert_eq!(&r[q.len() + 10..q.len() + 12], &[0, 4], "rdlength");
    }

    #[test]
    fn any_name_gets_the_same_answer() {
        for name in ["redpill.local", "captive.apple.com", "a.b.c.d.e.example.org"] {
            let r = answer(&query(name, 1), IP).unwrap();
            assert_eq!(&r[r.len() - 4..], &IP, "{name}");
        }
    }

    #[test]
    fn aaaa_and_other_types_get_an_empty_successful_answer() {
        for qtype in [28u16, 65, 15, 16] {
            let q = query("example.com", qtype);
            let r = answer(&q, IP).unwrap();
            assert_eq!(&r[6..8], &[0, 0], "no answer for type {qtype}");
            assert_eq!(r[3] & 0x0F, 0, "still NOERROR");
            assert_eq!(r.len(), q.len());
        }
    }

    #[test]
    fn any_query_is_answered_like_a() {
        let r = answer(&query("example.com", 255), IP).unwrap();
        assert_eq!(&r[6..8], &[0, 1]);
    }

    #[test]
    fn non_in_class_gets_no_answer() {
        let mut q = query("example.com", 1);
        let n = q.len();
        q[n - 1] = 3; // CHAOS
        assert_eq!(&answer(&q, IP).unwrap()[6..8], &[0, 0]);
    }

    #[test]
    fn recursion_desired_is_copied() {
        let mut q = query("example.com", 1);
        assert_eq!(answer(&q, IP).unwrap()[2] & 0x01, 1);
        q[2] = 0;
        assert_eq!(answer(&q, IP).unwrap()[2] & 0x01, 0);
    }

    #[test]
    fn only_the_first_question_is_answered() {
        let single = query("one.example", 1);
        let mut q = single.clone();
        q[5] = 2; // claims two questions
        q.extend_from_slice(&query("two.example", 1)[12..]);
        let r = answer(&q, IP).unwrap();
        assert_eq!(&r[4..6], &[0, 1]);
        assert_eq!(r.len(), single.len() + 16, "header + first question + one 16-byte answer");
    }

    #[test]
    fn garbage_and_non_queries_are_ignored() {
        assert_eq!(answer(&[], IP), None);
        assert_eq!(answer(&[0; 11], IP), None);
        // A response packet (QR set) must never be answered: it would loop with another responder.
        let mut resp = query("example.com", 1);
        resp[2] |= 0x80;
        assert_eq!(answer(&resp, IP), None);
        // Zero questions.
        let mut none = query("example.com", 1);
        none[5] = 0;
        assert_eq!(answer(&none, IP), None);
        // Non-standard opcode (status).
        let mut status = query("example.com", 1);
        status[2] |= 0x10;
        assert_eq!(answer(&status, IP), None);
        // Truncated inside the name and inside the type/class.
        let q = query("example.com", 1);
        for cut in 12..q.len() {
            assert_eq!(answer(&q[..cut], IP), None, "cut at {cut}");
        }
        // Compression pointer in the question.
        let mut ptr = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 0xC0, 0x0C, 0, 1, 0, 1];
        assert_eq!(answer(&ptr, IP), None);
        ptr.clear();
        assert_eq!(answer(&ptr, IP), None);
    }
}
