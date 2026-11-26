//! Month name recognition for listing dates.
//!
//! Matches English and many European month names and abbreviations, Russian
//! (UTF-8 or Windows-1251 bytes that were decoded as Latin-1), numeric months
//! carrying a non-ASCII suffix (CJK `月`, Korean `월`, or legacy Asian
//! encodings decoded as Latin-1), and tokens whose UTF-8 bytes were decoded as
//! Latin-1 because the rest of the line was not valid UTF-8.

/// Month (1–12) for a month token, or None.
///
/// Trailing `.` and `,` characters are ignored. Plain numbers are accepted
/// only when `allow_plain_numeric` is set; numbers followed only by
/// non-ASCII characters (`5月`, `12월`) are always accepted.
pub(crate) fn parse_month(token: &str, allow_plain_numeric: bool) -> Option<u32> {
    let t = token.trim_end_matches(['.', ',']);
    if t.is_empty() {
        return None;
    }
    if let Some(m) = direct(t, allow_plain_numeric) {
        return Some(m);
    }
    // Alternative interpretations of text that was decoded as Latin-1.
    if t.chars().all(|c| (c as u32) <= 0xFF) && t.chars().any(|c| (c as u32) >= 0x80) {
        let bytes: Vec<u8> = t.chars().map(|c| c as u32 as u8).collect();
        if let Ok(u) = std::str::from_utf8(&bytes) {
            if let Some(m) = direct(u, allow_plain_numeric) {
                return Some(m);
            }
        }
        let cyr: String = bytes.iter().map(|&b| cp1251_char(b)).collect();
        if let Some(m) = name_lookup(&cyr) {
            return Some(m);
        }
    }
    None
}

fn direct(t: &str, allow_plain_numeric: bool) -> Option<u32> {
    let num = t.trim_end_matches(|c: char| !c.is_ascii());
    let suffixed = num.len() != t.len();
    if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
        if num.len() > 2 || !(suffixed || allow_plain_numeric) {
            return None;
        }
        let v: u32 = num.parse().ok()?;
        return (1..=12).contains(&v).then_some(v);
    }
    name_lookup(t)
}

/// Windows-1251 byte to char, for the Cyrillic letter range only; other
/// bytes keep their Latin-1 meaning.
fn cp1251_char(b: u8) -> char {
    match b {
        0xC0..=0xFF => char::from_u32(0x0410 + u32::from(b - 0xC0)).unwrap_or('?'),
        0xA8 => 'Ё',
        0xB8 => 'ё',
        _ => char::from(b),
    }
}

fn name_lookup(t: &str) -> Option<u32> {
    let lower = t.to_lowercase();
    let m = match lower.as_str() {
        // January
        "jan" | "january" | "januar" | "jän" | "jänner" | "janv" | "janvier" | "gen"
        | "gennaio" | "ene" | "enero" | "janeiro" | "januari" | "sty" | "styczeń" | "styczen"
        | "stycznia" | "tammi" | "tammikuu" | "január" | "janúar" | "sau" | "sausis" | "sausio"
        | "янв" | "январь" | "января" => 1,
        // February
        "feb" | "february" | "februar" | "févr" | "fevr" | "fév" | "fev" | "février"
        | "fevrier" | "febbraio" | "febrero" | "fevereiro" | "februari" | "lut" | "luty"
        | "lutego" | "helmi" | "helmikuu" | "febr" | "február" | "febrúar" | "vas" | "vasaris"
        | "vasario" | "фев" | "февр" | "февраль" | "февраля" => 2,
        // March
        "mar" | "march" | "mär" | "märz" | "mrz" | "maerz" | "mars" | "marzo" | "março"
        | "marco" | "mrt" | "maart" | "marzec" | "marca" | "maalis" | "maaliskuu" | "márc"
        | "marc" | "március" | "marcius" | "kov" | "kovas" | "kovo" | "marts" | "мар" | "март"
        | "марта" => 3,
        // April
        "apr" | "april" | "avr" | "avril" | "aprile" | "abr" | "abril" | "kwi" | "kwiecień"
        | "kwiecien" | "kwietnia" | "huhti" | "huhtikuu" | "ápr" | "április" | "aprilis"
        | "apríl" | "bal" | "balandis" | "balandžio" | "апр" | "апрель" | "апреля" => {
            4
        }
        // May
        "may" | "mai" | "mag" | "maggio" | "mayo" | "maio" | "mei" | "maj" | "maja" | "touko"
        | "toukokuu" | "máj" | "május" | "majus" | "maí" | "geg" | "gegužė" | "gegužės" | "май"
        | "мая" => 5,
        // June
        "jun" | "june" | "juni" | "juin" | "giu" | "giugno" | "junio" | "junho" | "cze"
        | "czerwiec" | "czerwca" | "kesä" | "kesa" | "kesäkuu" | "kesakuu" | "jún" | "június"
        | "junius" | "júní" | "bir" | "birželis" | "birželio" | "июн" | "июнь" | "июня" => {
            6
        }
        // July
        "jul" | "july" | "juli" | "juil" | "juillet" | "lug" | "luglio" | "julio" | "julho"
        | "lip" | "lipiec" | "lipca" | "heinä" | "heina" | "heinäkuu" | "heinakuu" | "júl"
        | "július" | "julius" | "júlí" | "lie" | "liepa" | "liepos" | "июл" | "июль" | "июля" => {
            7
        }
        // August
        "aug" | "august" | "août" | "aout" | "ago" | "agosto" | "augustus" | "sie" | "sierpień"
        | "sierpien" | "sierpnia" | "elo" | "elokuu" | "augusztus" | "ágú" | "ágúst" | "agu"
        | "agust" | "rgp" | "rugp" | "rugpjūtis" | "rugpjūčio" | "augusti" | "авг" | "август"
        | "августа" => 8,
        // September
        "sep" | "sept" | "september" | "septembre" | "set" | "settembre" | "septiembre"
        | "setiembre" | "setembro" | "wrz" | "wrzesień" | "wrzesien" | "września" | "wrzesnia"
        | "syys" | "syyskuu" | "szept" | "szeptember" | "rgs" | "rugs" | "rugsėjis" | "rugsėjo"
        | "сен" | "сент" | "сентябрь" | "сентября" => 9,
        // October
        "oct" | "october" | "okt" | "oktober" | "octobre" | "ott" | "ottobre" | "octubre"
        | "out" | "outubro" | "paź" | "paz" | "październik" | "pazdziernik" | "października"
        | "pazdziernika" | "loka" | "lokakuu" | "október" | "spa" | "spalis" | "spalio" | "окт"
        | "октябрь" | "октября" => 10,
        // November
        "nov" | "november" | "novembre" | "noviembre" | "novembro" | "lis" | "listopad"
        | "listopada" | "marras" | "marraskuu" | "nóv" | "nóvember" | "lap" | "lapkritis"
        | "lapkričio" | "ноя" | "нояб" | "ноябрь" | "ноября" => 11,
        // December
        "dec" | "december" | "dez" | "dezember" | "déc" | "décembre" | "decembre" | "dic"
        | "dicembre" | "diciembre" | "dezembro" | "gru" | "grudzień" | "grudzien" | "grudnia"
        | "joulu" | "joulukuu" | "des" | "desember" | "grd" | "gruod" | "gruodis" | "gruodžio"
        | "дек" | "декабрь" | "декабря" => 12,
        _ => return None,
    };
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::parse_month;

    #[test]
    fn english_and_case() {
        assert_eq!(parse_month("Jan", false), Some(1));
        assert_eq!(parse_month("SEPT", false), Some(9));
        assert_eq!(parse_month("september,", false), Some(9));
        assert_eq!(parse_month("Dec.", false), Some(12));
        assert_eq!(parse_month("Foo", false), None);
        assert_eq!(parse_month("", false), None);
        assert_eq!(parse_month(".", false), None);
    }

    #[test]
    fn european() {
        assert_eq!(parse_month("Dez", false), Some(12));
        assert_eq!(parse_month("févr.", false), Some(2));
        assert_eq!(parse_month("fevr", false), Some(2));
        assert_eq!(parse_month("août", false), Some(8));
        assert_eq!(parse_month("aout", false), Some(8));
        assert_eq!(parse_month("déc", false), Some(12));
        assert_eq!(parse_month("März", false), Some(3));
        assert_eq!(parse_month("paź", false), Some(10));
        assert_eq!(parse_month("toukokuu", false), Some(5));
        assert_eq!(parse_month("kesä", false), Some(6));
        assert_eq!(parse_month("maí", false), Some(5));
        assert_eq!(parse_month("szept", false), Some(9));
        assert_eq!(parse_month("ágú", false), Some(8));
        assert_eq!(parse_month("rgs", false), Some(9));
        assert_eq!(parse_month("okt", false), Some(10));
        assert_eq!(parse_month("mrt", false), Some(3));
    }

    #[test]
    fn russian_all_encodings() {
        assert_eq!(parse_month("ноя", false), Some(11));
        assert_eq!(parse_month("НОЯ", false), Some(11));
        // Windows-1251 "янв" decoded as Latin-1.
        assert_eq!(parse_month("\u{FF}\u{ED}\u{E2}", false), Some(1));
        // Windows-1251 "ДЕК" decoded as Latin-1.
        assert_eq!(parse_month("\u{C4}\u{C5}\u{CA}", false), Some(12));
        // UTF-8 "фев" decoded as Latin-1.
        let mojibake: String = "фев".bytes().map(char::from).collect();
        assert_eq!(parse_month(&mojibake, false), Some(2));
        let fr: String = "févr".bytes().map(char::from).collect();
        assert_eq!(parse_month(&fr, false), Some(2));
    }

    #[test]
    fn numeric() {
        assert_eq!(parse_month("3月", false), Some(3));
        assert_eq!(parse_month("12월", false), Some(12));
        assert_eq!(parse_month("13月", false), None);
        assert_eq!(parse_month("3", false), None);
        assert_eq!(parse_month("03", true), Some(3));
        assert_eq!(parse_month("0", true), None);
        assert_eq!(parse_month("月", false), None);
        assert_eq!(parse_month("5\u{8c}\u{8e}", false), Some(5));
        assert_eq!(parse_month("Mär.,", false), Some(3));
    }
}
