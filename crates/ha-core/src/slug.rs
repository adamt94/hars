//! HA's `util.slugify`.

/// HA's `slugify`: lowercase ASCII, other runs become `_`, trimmed;
/// common Latin accents are transliterated, and nothing left is `unknown`
/// (but `""` stays `""`).
pub fn slugify(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut sep = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        let ascii: &str = match c {
            'a'..='z' | '0'..='9' => {
                if sep && !out.is_empty() {
                    out.push('_');
                }
                sep = false;
                out.push(c);
                continue;
            }
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => "a",
            'æ' => "ae",
            'ç' | 'ć' | 'č' => "c",
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' | 'ě' => "e",
            'ì' | 'í' | 'î' | 'ï' => "i",
            'ñ' | 'ń' | 'ň' => "n",
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => "o",
            'œ' => "oe",
            'ß' => "ss",
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' => "u",
            'ý' | 'ÿ' => "y",
            'ž' | 'ź' | 'ż' => "z",
            'š' | 'ś' => "s",
            'ł' => "l",
            'đ' | 'ð' => "d",
            'þ' => "th",
            'ř' => "r",
            _ => {
                sep = true;
                continue;
            }
        };
        if sep && !out.is_empty() {
            out.push('_');
        }
        sep = false;
        out.push_str(ascii);
    }
    if out.is_empty() {
        "unknown".into()
    } else {
        out
    }
}
