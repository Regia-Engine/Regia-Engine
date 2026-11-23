// SPDX-License-Identifier: MPL-2.0
//! The culture data of [`crate::loc_format`]: a compact table written by hand for Regia's shipped
//! locales (`en`, `ar`, `zh`), its debug locales (`en-XA`, `en-XD`) and about thirty of the most
//! used game locales, plus `root`.
//!
//! Derived from the public Unicode CLDR data (Unicode License v3; see `THIRD_PARTY_NOTICES` and
//! `docs/provenance.md`). Nothing here is a copy of a CLDR file: each value was typed in from the
//! published charts, and only the handful of fields Regia formats with are kept (Gregorian calendar
//! only, format-context month names, one date-time glue per language).
//!
//! Every field is optional except the tag: a lookup walks the locale's chain (`pt-BR` → `pt` →
//! `root`) and takes each field from the first entry that has it, so a region entry only lists what
//! differs from its language.

/// The decimal symbols and patterns of one numbering system.
#[derive(Debug)]
pub(crate) struct NumberData {
    /// Decimal separator.
    pub decimal: &'static str,
    /// Grouping separator.
    pub group: &'static str,
    /// Minus sign (some locales use U+2212 or carry bidi marks).
    pub minus: &'static str,
    /// Plus sign (for `signDisplay: "always"`).
    pub plus: &'static str,
    /// Size of the group nearest the decimal point.
    pub primary: u8,
    /// Size of every further group (2 for the Indian 3;2 grouping).
    pub secondary: u8,
    /// CLDR `minimumGroupingDigits`: 2 leaves four-digit numbers ungrouped (`es`: `1234`).
    pub min_grouping: u8,
    /// Percent pattern: `#` is the number (`#\u{a0}%`).
    pub percent: &'static str,
    /// Currency pattern: `#` is the number, `¤` the symbol (`¤#`, `#\u{a0}¤`).
    pub currency: &'static str,
    /// Negative currency pattern when it is not the minus sign before the positive one.
    pub currency_negative: Option<&'static str>,
    /// Text for NaN.
    pub nan: &'static str,
}

/// A locale's native digits (opt-in with `numberingSystem: "native"`).
#[derive(Debug)]
pub(crate) struct NativeData {
    /// The CLDR numbering system id (`arab`, `deva`, `thai`).
    pub system: &'static str,
    /// The digits zero to nine.
    pub digits: [char; 10],
    /// Symbols of the native system when they differ from the Latin ones.
    pub number: Option<&'static NumberData>,
}

/// Month, weekday and day-period names (format context, Gregorian).
#[derive(Debug)]
pub(crate) struct CalendarData {
    /// January first.
    pub months_wide: [&'static str; 12],
    /// January first.
    pub months_abbr: [&'static str; 12],
    /// Sunday first.
    pub weekdays_wide: [&'static str; 7],
    /// Sunday first.
    pub weekdays_abbr: [&'static str; 7],
    /// Before noon.
    pub am: &'static str,
    /// After noon.
    pub pm: &'static str,
}

/// Date and time patterns (a subset of the UTS #35 pattern letters, see `loc_format`).
#[derive(Debug)]
pub(crate) struct DatePatterns {
    /// `dateStyle: "short"`.
    pub short: &'static str,
    /// `dateStyle: "medium"`.
    pub medium: &'static str,
    /// `dateStyle: "long"`.
    pub long: &'static str,
    /// `dateStyle: "full"`.
    pub full: &'static str,
    /// Short time on a 12-hour clock.
    pub time12: &'static str,
    /// Short time on a 24-hour clock.
    pub time24: &'static str,
    /// Date-time glue: `{1}` is the date, `{0}` the time.
    pub glue: &'static str,
    /// Prefix of a UTC offset in long times (`GMT+1`).
    pub gmt: &'static str,
}

/// One list pattern set: `two` joins a pair, `middle` all but the last of longer lists, `end` the
/// last.
#[derive(Debug)]
pub(crate) struct ListPatterns {
    /// Between the two items of a pair.
    pub two: &'static str,
    /// Between items before the last.
    pub middle: &'static str,
    /// Before the last item of three or more.
    pub end: &'static str,
}

/// `and` and `or` lists.
#[derive(Debug)]
pub(crate) struct ListData {
    /// Conjunction ("a, b, and c").
    pub and: ListPatterns,
    /// Disjunction ("a, b, or c").
    pub or: ListPatterns,
}

/// Relative-time data: `{0}` is the formatted number, `{1}` the unit noun for its plural category.
#[derive(Debug)]
pub(crate) struct RelativeData {
    /// Future template.
    pub future: &'static str,
    /// Past template.
    pub past: &'static str,
    /// Unit nouns by plural category, in `RelativeUnit` order (second, minute, hour, day, week,
    /// month, year). The `other` form is required.
    pub units: [&'static [(&'static str, &'static str)]; 7],
    /// Yesterday, today, tomorrow (`numeric: "auto"`).
    pub days: Option<[&'static str; 3]>,
}

/// One table row.
#[derive(Debug)]
pub(crate) struct LocaleData {
    /// Canonical tag (`pt`, `zh-Hant`, `es-419`, `root`).
    pub tag: &'static str,
    /// The CLDR parent when it is not the tag minus its last subtag.
    pub parent: Option<&'static str>,
    /// Latin-digit number symbols and patterns.
    pub number: Option<&'static NumberData>,
    /// Native digits.
    pub native: Option<&'static NativeData>,
    /// Names.
    pub calendar: Option<&'static CalendarData>,
    /// Patterns.
    pub dates: Option<&'static DatePatterns>,
    /// Whether the locale's default clock is 12-hour.
    pub hour12: Option<bool>,
    /// Currency symbols that differ from the currency table's default in this locale.
    pub currencies: &'static [(&'static str, &'static str)],
    /// Lists.
    pub list: Option<&'static ListData>,
    /// Relative time.
    pub relative: Option<&'static RelativeData>,
}

const EMPTY: LocaleData = LocaleData {
    tag: "",
    parent: None,
    number: None,
    native: None,
    calendar: None,
    dates: None,
    hour12: None,
    currencies: &[],
    list: None,
    relative: None,
};

/// One currency: ISO 4217 code, default fraction digits, default symbol, narrow symbol.
#[derive(Debug)]
pub(crate) struct Currency {
    /// ISO 4217 code.
    pub code: &'static str,
    /// Fraction digits.
    pub digits: u8,
    /// The symbol outside the currency's home locales (`US$`).
    pub symbol: &'static str,
    /// The narrow symbol (`$`).
    pub narrow: &'static str,
}

const fn cur(code: &'static str, digits: u8, symbol: &'static str, narrow: &'static str) -> Currency {
    Currency {
        code,
        digits,
        symbol,
        narrow,
    }
}

/// Known currencies. A well-formed code not listed here formats with 2 digits and the code as its
/// symbol.
pub(crate) static CURRENCIES: &[Currency] = &[
    cur("AED", 2, "AED", "AED"),
    cur("ARS", 2, "ARS", "$"),
    cur("AUD", 2, "A$", "$"),
    cur("BHD", 3, "BHD", "BHD"),
    cur("BRL", 2, "R$", "R$"),
    cur("CAD", 2, "CA$", "$"),
    cur("CHF", 2, "CHF", "CHF"),
    cur("CLP", 0, "CLP", "$"),
    cur("CNY", 2, "CN¥", "¥"),
    cur("COP", 2, "COP", "$"),
    cur("CZK", 2, "CZK", "Kč"),
    cur("DKK", 2, "DKK", "kr"),
    cur("EUR", 2, "€", "€"),
    cur("GBP", 2, "£", "£"),
    cur("HKD", 2, "HK$", "$"),
    cur("HUF", 2, "HUF", "Ft"),
    cur("IDR", 2, "IDR", "Rp"),
    cur("ILS", 2, "₪", "₪"),
    cur("INR", 2, "₹", "₹"),
    cur("JPY", 0, "JP¥", "¥"),
    cur("KRW", 0, "₩", "₩"),
    cur("KWD", 3, "KWD", "KWD"),
    cur("MXN", 2, "MX$", "$"),
    cur("NOK", 2, "NOK", "kr"),
    cur("NZD", 2, "NZ$", "$"),
    cur("PLN", 2, "PLN", "zł"),
    cur("RON", 2, "RON", "lei"),
    cur("RUB", 2, "RUB", "₽"),
    cur("SAR", 2, "SAR", "SAR"),
    cur("SEK", 2, "SEK", "kr"),
    cur("SGD", 2, "SGD", "$"),
    cur("THB", 2, "THB", "฿"),
    cur("TRY", 2, "TRY", "₺"),
    cur("TWD", 2, "NT$", "$"),
    cur("UAH", 2, "UAH", "₴"),
    cur("USD", 2, "US$", "$"),
    cur("VND", 0, "₫", "₫"),
    cur("ZAR", 2, "ZAR", "R"),
];

// --- Number data --------------------------------------------------------------------------------

const fn num(
    decimal: &'static str,
    group: &'static str,
    percent: &'static str,
    currency: &'static str,
) -> NumberData {
    NumberData {
        decimal,
        group,
        minus: "-",
        plus: "+",
        primary: 3,
        secondary: 3,
        min_grouping: 1,
        percent,
        currency,
        currency_negative: None,
        nan: "NaN",
    }
}

const NBSP: &str = "\u{a0}";

pub(crate) static ROOT_NUM: NumberData = num(".", ",", "#%", "¤\u{a0}#");
static EN_NUM: NumberData = num(".", ",", "#%", "¤#");
static IN_NUM: NumberData = NumberData {
    secondary: 2,
    ..num(".", ",", "#%", "¤#")
};
static DE_NUM: NumberData = num(",", ".", "#\u{a0}%", "#\u{a0}¤");
static FR_NUM: NumberData = num(",", "\u{202f}", "#\u{202f}%", "#\u{a0}¤");
static FR_CA_NUM: NumberData = num(",", NBSP, "#\u{a0}%", "#\u{a0}¤");
static ES_NUM: NumberData = NumberData {
    min_grouping: 2,
    ..num(",", ".", "#\u{a0}%", "#\u{a0}¤")
};
static ES_419_NUM: NumberData = num(".", ",", "#\u{a0}%", "¤#");
static IT_NUM: NumberData = num(",", ".", "#%", "#\u{a0}¤");
static PT_NUM: NumberData = num(",", ".", "#%", "¤\u{a0}#");
static PT_PT_NUM: NumberData = NumberData {
    min_grouping: 2,
    ..num(",", NBSP, "#%", "#\u{a0}¤")
};
static RU_NUM: NumberData = NumberData {
    nan: "не\u{a0}число",
    ..num(",", NBSP, "#\u{a0}%", "#\u{a0}¤")
};
static PL_NUM: NumberData = NumberData {
    min_grouping: 2,
    ..num(",", NBSP, "#%", "#\u{a0}¤")
};
static TR_NUM: NumberData = num(",", ".", "%#", "¤#");
static AR_NUM: NumberData = NumberData {
    minus: "\u{200e}-",
    plus: "\u{200e}+",
    ..num(".", ",", "#\u{200e}%\u{200e}", "\u{200f}#\u{a0}¤")
};
static AR_ARAB_NUM: NumberData = NumberData {
    minus: "\u{61c}-",
    plus: "\u{61c}+",
    ..num("\u{66b}", "\u{66c}", "#\u{66a}\u{61c}", "\u{200f}#\u{a0}¤")
};
static VI_NUM: NumberData = num(",", ".", "#%", "#\u{a0}¤");
static ID_NUM: NumberData = num(",", ".", "#%", "¤#");
static NL_NUM: NumberData = NumberData {
    currency_negative: Some("¤\u{a0}-#"),
    ..num(",", ".", "#%", "¤\u{a0}#")
};
static NORDIC_MINUS_NUM: NumberData = NumberData {
    minus: "\u{2212}",
    ..num(",", NBSP, "#\u{a0}%", "#\u{a0}¤")
};
static DA_NUM: NumberData = num(",", ".", "#\u{a0}%", "#\u{a0}¤");
static CS_NUM: NumberData = num(",", NBSP, "#\u{a0}%", "#\u{a0}¤");
static SPACE_GROUP_NUM: NumberData = num(",", NBSP, "#%", "#\u{a0}¤");
static EL_NUM: NumberData = num(",", ".", "#%", "#\u{a0}¤");
static RO_NUM: NumberData = num(",", ".", "#\u{a0}%", "#\u{a0}¤");

static ARAB_DIGITS: NativeData = NativeData {
    system: "arab",
    digits: ['٠', '١', '٢', '٣', '٤', '٥', '٦', '٧', '٨', '٩'],
    number: Some(&AR_ARAB_NUM),
};
static DEVA_DIGITS: NativeData = NativeData {
    system: "deva",
    digits: ['०', '१', '२', '३', '४', '५', '६', '७', '८', '९'],
    number: None,
};
static THAI_DIGITS: NativeData = NativeData {
    system: "thai",
    digits: ['๐', '๑', '๒', '๓', '๔', '๕', '๖', '๗', '๘', '๙'],
    number: None,
};

// --- Calendars ----------------------------------------------------------------------------------

pub(crate) static ROOT_CAL: CalendarData = CalendarData {
    months_wide: [
        "M01", "M02", "M03", "M04", "M05", "M06", "M07", "M08", "M09", "M10", "M11", "M12",
    ],
    months_abbr: [
        "M01", "M02", "M03", "M04", "M05", "M06", "M07", "M08", "M09", "M10", "M11", "M12",
    ],
    weekdays_wide: ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    weekdays_abbr: ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    am: "AM",
    pm: "PM",
};
static EN_CAL: CalendarData = CalendarData {
    months_wide: [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ],
    months_abbr: [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ],
    weekdays_wide: [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ],
    weekdays_abbr: ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    am: "AM",
    pm: "PM",
};
static FR_CAL: CalendarData = CalendarData {
    months_wide: [
        "janvier",
        "février",
        "mars",
        "avril",
        "mai",
        "juin",
        "juillet",
        "août",
        "septembre",
        "octobre",
        "novembre",
        "décembre",
    ],
    months_abbr: [
        "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.", "déc.",
    ],
    weekdays_wide: [
        "dimanche", "lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi",
    ],
    weekdays_abbr: ["dim.", "lun.", "mar.", "mer.", "jeu.", "ven.", "sam."],
    am: "AM",
    pm: "PM",
};
static DE_CAL: CalendarData = CalendarData {
    months_wide: [
        "Januar",
        "Februar",
        "März",
        "April",
        "Mai",
        "Juni",
        "Juli",
        "August",
        "September",
        "Oktober",
        "November",
        "Dezember",
    ],
    months_abbr: [
        "Jan.", "Feb.", "März", "Apr.", "Mai", "Juni", "Juli", "Aug.", "Sept.", "Okt.", "Nov.", "Dez.",
    ],
    weekdays_wide: [
        "Sonntag",
        "Montag",
        "Dienstag",
        "Mittwoch",
        "Donnerstag",
        "Freitag",
        "Samstag",
    ],
    weekdays_abbr: ["So.", "Mo.", "Di.", "Mi.", "Do.", "Fr.", "Sa."],
    am: "AM",
    pm: "PM",
};
static ES_CAL: CalendarData = CalendarData {
    months_wide: [
        "enero",
        "febrero",
        "marzo",
        "abril",
        "mayo",
        "junio",
        "julio",
        "agosto",
        "septiembre",
        "octubre",
        "noviembre",
        "diciembre",
    ],
    months_abbr: [
        "ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sept", "oct", "nov", "dic",
    ],
    weekdays_wide: [
        "domingo",
        "lunes",
        "martes",
        "miércoles",
        "jueves",
        "viernes",
        "sábado",
    ],
    weekdays_abbr: ["dom", "lun", "mar", "mié", "jue", "vie", "sáb"],
    am: "a.\u{a0}m.",
    pm: "p.\u{a0}m.",
};
static IT_CAL: CalendarData = CalendarData {
    months_wide: [
        "gennaio",
        "febbraio",
        "marzo",
        "aprile",
        "maggio",
        "giugno",
        "luglio",
        "agosto",
        "settembre",
        "ottobre",
        "novembre",
        "dicembre",
    ],
    months_abbr: [
        "gen", "feb", "mar", "apr", "mag", "giu", "lug", "ago", "set", "ott", "nov", "dic",
    ],
    weekdays_wide: [
        "domenica",
        "lunedì",
        "martedì",
        "mercoledì",
        "giovedì",
        "venerdì",
        "sabato",
    ],
    weekdays_abbr: ["dom", "lun", "mar", "mer", "gio", "ven", "sab"],
    am: "AM",
    pm: "PM",
};
static PT_CAL: CalendarData = CalendarData {
    months_wide: [
        "janeiro",
        "fevereiro",
        "março",
        "abril",
        "maio",
        "junho",
        "julho",
        "agosto",
        "setembro",
        "outubro",
        "novembro",
        "dezembro",
    ],
    months_abbr: [
        "jan.", "fev.", "mar.", "abr.", "mai.", "jun.", "jul.", "ago.", "set.", "out.", "nov.", "dez.",
    ],
    weekdays_wide: [
        "domingo",
        "segunda-feira",
        "terça-feira",
        "quarta-feira",
        "quinta-feira",
        "sexta-feira",
        "sábado",
    ],
    weekdays_abbr: ["dom.", "seg.", "ter.", "qua.", "qui.", "sex.", "sáb."],
    am: "AM",
    pm: "PM",
};
static RU_CAL: CalendarData = CalendarData {
    months_wide: [
        "января",
        "февраля",
        "марта",
        "апреля",
        "мая",
        "июня",
        "июля",
        "августа",
        "сентября",
        "октября",
        "ноября",
        "декабря",
    ],
    months_abbr: [
        "янв.",
        "февр.",
        "мар.",
        "апр.",
        "мая",
        "июн.",
        "июл.",
        "авг.",
        "сент.",
        "окт.",
        "нояб.",
        "дек.",
    ],
    weekdays_wide: [
        "воскресенье",
        "понедельник",
        "вторник",
        "среда",
        "четверг",
        "пятница",
        "суббота",
    ],
    weekdays_abbr: ["вс", "пн", "вт", "ср", "чт", "пт", "сб"],
    am: "AM",
    pm: "PM",
};
static PL_CAL: CalendarData = CalendarData {
    months_wide: [
        "stycznia",
        "lutego",
        "marca",
        "kwietnia",
        "maja",
        "czerwca",
        "lipca",
        "sierpnia",
        "września",
        "października",
        "listopada",
        "grudnia",
    ],
    months_abbr: [
        "sty", "lut", "mar", "kwi", "maj", "cze", "lip", "sie", "wrz", "paź", "lis", "gru",
    ],
    weekdays_wide: [
        "niedziela",
        "poniedziałek",
        "wtorek",
        "środa",
        "czwartek",
        "piątek",
        "sobota",
    ],
    weekdays_abbr: ["niedz.", "pon.", "wt.", "śr.", "czw.", "pt.", "sob."],
    am: "AM",
    pm: "PM",
};
static JA_CAL: CalendarData = CalendarData {
    months_wide: [
        "1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月",
    ],
    months_abbr: [
        "1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月",
    ],
    weekdays_wide: [
        "日曜日",
        "月曜日",
        "火曜日",
        "水曜日",
        "木曜日",
        "金曜日",
        "土曜日",
    ],
    weekdays_abbr: ["日", "月", "火", "水", "木", "金", "土"],
    am: "午前",
    pm: "午後",
};
static KO_CAL: CalendarData = CalendarData {
    months_wide: [
        "1월", "2월", "3월", "4월", "5월", "6월", "7월", "8월", "9월", "10월", "11월", "12월",
    ],
    months_abbr: [
        "1월", "2월", "3월", "4월", "5월", "6월", "7월", "8월", "9월", "10월", "11월", "12월",
    ],
    weekdays_wide: [
        "일요일",
        "월요일",
        "화요일",
        "수요일",
        "목요일",
        "금요일",
        "토요일",
    ],
    weekdays_abbr: ["일", "월", "화", "수", "목", "금", "토"],
    am: "오전",
    pm: "오후",
};
static ZH_CAL: CalendarData = CalendarData {
    months_wide: [
        "一月",
        "二月",
        "三月",
        "四月",
        "五月",
        "六月",
        "七月",
        "八月",
        "九月",
        "十月",
        "十一月",
        "十二月",
    ],
    months_abbr: [
        "1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月",
    ],
    weekdays_wide: [
        "星期日",
        "星期一",
        "星期二",
        "星期三",
        "星期四",
        "星期五",
        "星期六",
    ],
    weekdays_abbr: ["周日", "周一", "周二", "周三", "周四", "周五", "周六"],
    am: "上午",
    pm: "下午",
};
static ZH_HANT_CAL: CalendarData = CalendarData {
    months_wide: [
        "1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月",
    ],
    months_abbr: [
        "1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月",
    ],
    weekdays_wide: [
        "星期日",
        "星期一",
        "星期二",
        "星期三",
        "星期四",
        "星期五",
        "星期六",
    ],
    weekdays_abbr: ["週日", "週一", "週二", "週三", "週四", "週五", "週六"],
    am: "上午",
    pm: "下午",
};
static TR_CAL: CalendarData = CalendarData {
    months_wide: [
        "Ocak", "Şubat", "Mart", "Nisan", "Mayıs", "Haziran", "Temmuz", "Ağustos", "Eylül", "Ekim", "Kasım",
        "Aralık",
    ],
    months_abbr: [
        "Oca", "Şub", "Mar", "Nis", "May", "Haz", "Tem", "Ağu", "Eyl", "Eki", "Kas", "Ara",
    ],
    weekdays_wide: [
        "Pazar",
        "Pazartesi",
        "Salı",
        "Çarşamba",
        "Perşembe",
        "Cuma",
        "Cumartesi",
    ],
    weekdays_abbr: ["Paz", "Pzt", "Sal", "Çar", "Per", "Cum", "Cmt"],
    am: "ÖÖ",
    pm: "ÖS",
};
static AR_CAL: CalendarData = CalendarData {
    months_wide: [
        "يناير",
        "فبراير",
        "مارس",
        "أبريل",
        "مايو",
        "يونيو",
        "يوليو",
        "أغسطس",
        "سبتمبر",
        "أكتوبر",
        "نوفمبر",
        "ديسمبر",
    ],
    months_abbr: [
        "يناير",
        "فبراير",
        "مارس",
        "أبريل",
        "مايو",
        "يونيو",
        "يوليو",
        "أغسطس",
        "سبتمبر",
        "أكتوبر",
        "نوفمبر",
        "ديسمبر",
    ],
    weekdays_wide: [
        "الأحد",
        "الاثنين",
        "الثلاثاء",
        "الأربعاء",
        "الخميس",
        "الجمعة",
        "السبت",
    ],
    weekdays_abbr: [
        "الأحد",
        "الاثنين",
        "الثلاثاء",
        "الأربعاء",
        "الخميس",
        "الجمعة",
        "السبت",
    ],
    am: "ص",
    pm: "م",
};
static TH_CAL: CalendarData = CalendarData {
    months_wide: [
        "มกราคม",
        "กุมภาพันธ์",
        "มีนาคม",
        "เมษายน",
        "พฤษภาคม",
        "มิถุนายน",
        "กรกฎาคม",
        "สิงหาคม",
        "กันยายน",
        "ตุลาคม",
        "พฤศจิกายน",
        "ธันวาคม",
    ],
    months_abbr: [
        "ม.ค.",
        "ก.พ.",
        "มี.ค.",
        "เม.ย.",
        "พ.ค.",
        "มิ.ย.",
        "ก.ค.",
        "ส.ค.",
        "ก.ย.",
        "ต.ค.",
        "พ.ย.",
        "ธ.ค.",
    ],
    weekdays_wide: [
        "วันอาทิตย์",
        "วันจันทร์",
        "วันอังคาร",
        "วันพุธ",
        "วันพฤหัสบดี",
        "วันศุกร์",
        "วันเสาร์",
    ],
    weekdays_abbr: ["อา.", "จ.", "อ.", "พ.", "พฤ.", "ศ.", "ส."],
    am: "ก่อนเที่ยง",
    pm: "หลังเที่ยง",
};
static VI_CAL: CalendarData = CalendarData {
    months_wide: [
        "tháng 1",
        "tháng 2",
        "tháng 3",
        "tháng 4",
        "tháng 5",
        "tháng 6",
        "tháng 7",
        "tháng 8",
        "tháng 9",
        "tháng 10",
        "tháng 11",
        "tháng 12",
    ],
    months_abbr: [
        "thg 1", "thg 2", "thg 3", "thg 4", "thg 5", "thg 6", "thg 7", "thg 8", "thg 9", "thg 10", "thg 11",
        "thg 12",
    ],
    weekdays_wide: [
        "Chủ Nhật",
        "Thứ Hai",
        "Thứ Ba",
        "Thứ Tư",
        "Thứ Năm",
        "Thứ Sáu",
        "Thứ Bảy",
    ],
    weekdays_abbr: ["CN", "Th 2", "Th 3", "Th 4", "Th 5", "Th 6", "Th 7"],
    am: "SA",
    pm: "CH",
};
static ID_CAL: CalendarData = CalendarData {
    months_wide: [
        "Januari",
        "Februari",
        "Maret",
        "April",
        "Mei",
        "Juni",
        "Juli",
        "Agustus",
        "September",
        "Oktober",
        "November",
        "Desember",
    ],
    months_abbr: [
        "Jan", "Feb", "Mar", "Apr", "Mei", "Jun", "Jul", "Agu", "Sep", "Okt", "Nov", "Des",
    ],
    weekdays_wide: ["Minggu", "Senin", "Selasa", "Rabu", "Kamis", "Jumat", "Sabtu"],
    weekdays_abbr: ["Min", "Sen", "Sel", "Rab", "Kam", "Jum", "Sab"],
    am: "AM",
    pm: "PM",
};
static NL_CAL: CalendarData = CalendarData {
    months_wide: [
        "januari",
        "februari",
        "maart",
        "april",
        "mei",
        "juni",
        "juli",
        "augustus",
        "september",
        "oktober",
        "november",
        "december",
    ],
    months_abbr: [
        "jan", "feb", "mrt", "apr", "mei", "jun", "jul", "aug", "sep", "okt", "nov", "dec",
    ],
    weekdays_wide: [
        "zondag",
        "maandag",
        "dinsdag",
        "woensdag",
        "donderdag",
        "vrijdag",
        "zaterdag",
    ],
    weekdays_abbr: ["zo", "ma", "di", "wo", "do", "vr", "za"],
    am: "a.m.",
    pm: "p.m.",
};
static SV_CAL: CalendarData = CalendarData {
    months_wide: [
        "januari",
        "februari",
        "mars",
        "april",
        "maj",
        "juni",
        "juli",
        "augusti",
        "september",
        "oktober",
        "november",
        "december",
    ],
    months_abbr: [
        "jan.", "feb.", "mars", "apr.", "maj", "juni", "juli", "aug.", "sep.", "okt.", "nov.", "dec.",
    ],
    weekdays_wide: [
        "söndag", "måndag", "tisdag", "onsdag", "torsdag", "fredag", "lördag",
    ],
    weekdays_abbr: ["sön", "mån", "tis", "ons", "tors", "fre", "lör"],
    am: "fm",
    pm: "em",
};
static DA_CAL: CalendarData = CalendarData {
    months_wide: [
        "januar",
        "februar",
        "marts",
        "april",
        "maj",
        "juni",
        "juli",
        "august",
        "september",
        "oktober",
        "november",
        "december",
    ],
    months_abbr: [
        "jan.", "feb.", "mar.", "apr.", "maj", "jun.", "jul.", "aug.", "sep.", "okt.", "nov.", "dec.",
    ],
    weekdays_wide: [
        "søndag", "mandag", "tirsdag", "onsdag", "torsdag", "fredag", "lørdag",
    ],
    weekdays_abbr: ["søn.", "man.", "tirs.", "ons.", "tors.", "fre.", "lør."],
    am: "AM",
    pm: "PM",
};
static NB_CAL: CalendarData = CalendarData {
    months_wide: [
        "januar",
        "februar",
        "mars",
        "april",
        "mai",
        "juni",
        "juli",
        "august",
        "september",
        "oktober",
        "november",
        "desember",
    ],
    months_abbr: [
        "jan.", "feb.", "mar.", "apr.", "mai", "jun.", "jul.", "aug.", "sep.", "okt.", "nov.", "des.",
    ],
    weekdays_wide: [
        "søndag", "mandag", "tirsdag", "onsdag", "torsdag", "fredag", "lørdag",
    ],
    weekdays_abbr: ["søn.", "man.", "tir.", "ons.", "tor.", "fre.", "lør."],
    am: "a.m.",
    pm: "p.m.",
};
static FI_CAL: CalendarData = CalendarData {
    months_wide: [
        "tammikuuta",
        "helmikuuta",
        "maaliskuuta",
        "huhtikuuta",
        "toukokuuta",
        "kesäkuuta",
        "heinäkuuta",
        "elokuuta",
        "syyskuuta",
        "lokakuuta",
        "marraskuuta",
        "joulukuuta",
    ],
    months_abbr: [
        "tammik.", "helmik.", "maalisk.", "huhtik.", "toukok.", "kesäk.", "heinäk.", "elok.", "syysk.",
        "lokak.", "marrask.", "jouluk.",
    ],
    weekdays_wide: [
        "sunnuntai",
        "maanantai",
        "tiistai",
        "keskiviikko",
        "torstai",
        "perjantai",
        "lauantai",
    ],
    weekdays_abbr: ["su", "ma", "ti", "ke", "to", "pe", "la"],
    am: "ap.",
    pm: "ip.",
};
static CS_CAL: CalendarData = CalendarData {
    months_wide: [
        "ledna",
        "února",
        "března",
        "dubna",
        "května",
        "června",
        "července",
        "srpna",
        "září",
        "října",
        "listopadu",
        "prosince",
    ],
    months_abbr: [
        "led", "úno", "bře", "dub", "kvě", "čvn", "čvc", "srp", "zář", "říj", "lis", "pro",
    ],
    weekdays_wide: [
        "neděle",
        "pondělí",
        "úterý",
        "středa",
        "čtvrtek",
        "pátek",
        "sobota",
    ],
    weekdays_abbr: ["ne", "po", "út", "st", "čt", "pá", "so"],
    am: "dop.",
    pm: "odp.",
};
static HU_CAL: CalendarData = CalendarData {
    months_wide: [
        "január",
        "február",
        "március",
        "április",
        "május",
        "június",
        "július",
        "augusztus",
        "szeptember",
        "október",
        "november",
        "december",
    ],
    months_abbr: [
        "jan.", "febr.", "márc.", "ápr.", "máj.", "jún.", "júl.", "aug.", "szept.", "okt.", "nov.", "dec.",
    ],
    weekdays_wide: [
        "vasárnap",
        "hétfő",
        "kedd",
        "szerda",
        "csütörtök",
        "péntek",
        "szombat",
    ],
    weekdays_abbr: ["V", "H", "K", "Sze", "Cs", "P", "Szo"],
    am: "de.",
    pm: "du.",
};
static UK_CAL: CalendarData = CalendarData {
    months_wide: [
        "січня",
        "лютого",
        "березня",
        "квітня",
        "травня",
        "червня",
        "липня",
        "серпня",
        "вересня",
        "жовтня",
        "листопада",
        "грудня",
    ],
    months_abbr: [
        "січ.",
        "лют.",
        "бер.",
        "квіт.",
        "трав.",
        "черв.",
        "лип.",
        "серп.",
        "вер.",
        "жовт.",
        "лист.",
        "груд.",
    ],
    weekdays_wide: [
        "неділя",
        "понеділок",
        "вівторок",
        "середа",
        "четвер",
        "пʼятниця",
        "субота",
    ],
    weekdays_abbr: ["нд", "пн", "вт", "ср", "чт", "пт", "сб"],
    am: "дп",
    pm: "пп",
};
static EL_CAL: CalendarData = CalendarData {
    months_wide: [
        "Ιανουαρίου",
        "Φεβρουαρίου",
        "Μαρτίου",
        "Απριλίου",
        "Μαΐου",
        "Ιουνίου",
        "Ιουλίου",
        "Αυγούστου",
        "Σεπτεμβρίου",
        "Οκτωβρίου",
        "Νοεμβρίου",
        "Δεκεμβρίου",
    ],
    months_abbr: [
        "Ιαν", "Φεβ", "Μαρ", "Απρ", "Μαΐ", "Ιουν", "Ιουλ", "Αυγ", "Σεπ", "Οκτ", "Νοε", "Δεκ",
    ],
    weekdays_wide: [
        "Κυριακή",
        "Δευτέρα",
        "Τρίτη",
        "Τετάρτη",
        "Πέμπτη",
        "Παρασκευή",
        "Σάββατο",
    ],
    weekdays_abbr: ["Κυρ", "Δευ", "Τρί", "Τετ", "Πέμ", "Παρ", "Σάβ"],
    am: "π.μ.",
    pm: "μ.μ.",
};
static RO_CAL: CalendarData = CalendarData {
    months_wide: [
        "ianuarie",
        "februarie",
        "martie",
        "aprilie",
        "mai",
        "iunie",
        "iulie",
        "august",
        "septembrie",
        "octombrie",
        "noiembrie",
        "decembrie",
    ],
    months_abbr: [
        "ian.", "feb.", "mar.", "apr.", "mai", "iun.", "iul.", "aug.", "sept.", "oct.", "nov.", "dec.",
    ],
    weekdays_wide: [
        "duminică",
        "luni",
        "marți",
        "miercuri",
        "joi",
        "vineri",
        "sâmbătă",
    ],
    weekdays_abbr: ["dum.", "lun.", "mar.", "mie.", "joi", "vin.", "sâm."],
    am: "a.m.",
    pm: "p.m.",
};
static HI_CAL: CalendarData = CalendarData {
    months_wide: [
        "जनवरी",
        "फ़रवरी",
        "मार्च",
        "अप्रैल",
        "मई",
        "जून",
        "जुलाई",
        "अगस्त",
        "सितंबर",
        "अक्तूबर",
        "नवंबर",
        "दिसंबर",
    ],
    months_abbr: [
        "जन॰",
        "फ़र॰",
        "मार्च",
        "अप्रैल",
        "मई",
        "जून",
        "जुल॰",
        "अग॰",
        "सित॰",
        "अक्तू॰",
        "नव॰",
        "दिस॰",
    ],
    weekdays_wide: ["रविवार", "सोमवार", "मंगलवार", "बुधवार", "गुरुवार", "शुक्रवार", "शनिवार"],
    weekdays_abbr: ["रवि", "सोम", "मंगल", "बुध", "गुरु", "शुक्र", "शनि"],
    am: "am",
    pm: "pm",
};

// --- Date patterns ------------------------------------------------------------------------------

const fn dates(
    short: &'static str,
    medium: &'static str,
    long: &'static str,
    full: &'static str,
    time12: &'static str,
    time24: &'static str,
    glue: &'static str,
) -> DatePatterns {
    DatePatterns {
        short,
        medium,
        long,
        full,
        time12,
        time24,
        glue,
        gmt: "GMT",
    }
}

pub(crate) static ROOT_DATES: DatePatterns = dates(
    "y-MM-dd",
    "y MMM d",
    "y MMMM d",
    "y MMMM d, EEEE",
    "h:mm a",
    "HH:mm",
    "{1} {0}",
);
static EN_DATES: DatePatterns = dates(
    "M/d/yy",
    "MMM d, y",
    "MMMM d, y",
    "EEEE, MMMM d, y",
    "h:mm\u{202f}a",
    "HH:mm",
    "{1}, {0}",
);
static EN_001_DATES: DatePatterns = dates(
    "dd/MM/y",
    "d MMM y",
    "d MMMM y",
    "EEEE d MMMM y",
    "h:mm\u{202f}a",
    "HH:mm",
    "{1}, {0}",
);
static FR_DATES: DatePatterns = DatePatterns {
    gmt: "UTC",
    ..dates(
        "dd/MM/y",
        "d MMM y",
        "d MMMM y",
        "EEEE d MMMM y",
        "h:mm a",
        "HH:mm",
        "{1} {0}",
    )
};
static FR_CA_DATES: DatePatterns = DatePatterns {
    gmt: "UTC",
    ..dates(
        "y-MM-dd",
        "d MMM y",
        "d MMMM y",
        "EEEE d MMMM y",
        "h:mm a",
        "HH:mm",
        "{1} {0}",
    )
};
static DE_DATES: DatePatterns = dates(
    "dd.MM.yy",
    "dd.MM.y",
    "d. MMMM y",
    "EEEE, d. MMMM y",
    "h:mm\u{202f}a",
    "HH:mm",
    "{1}, {0}",
);
static ES_DATES: DatePatterns = dates(
    "d/M/yy",
    "d MMM y",
    "d 'de' MMMM 'de' y",
    "EEEE, d 'de' MMMM 'de' y",
    "h:mm\u{a0}a",
    "H:mm",
    "{1}, {0}",
);
static ES_MX_DATES: DatePatterns = dates(
    "dd/MM/yy",
    "d MMM y",
    "d 'de' MMMM 'de' y",
    "EEEE, d 'de' MMMM 'de' y",
    "h:mm\u{a0}a",
    "H:mm",
    "{1}, {0}",
);
static IT_DATES: DatePatterns = dates(
    "dd/MM/yy",
    "d MMM y",
    "d MMMM y",
    "EEEE d MMMM y",
    "h:mm\u{202f}a",
    "HH:mm",
    "{1}, {0}",
);
static PT_DATES: DatePatterns = dates(
    "dd/MM/y",
    "d 'de' MMM 'de' y",
    "d 'de' MMMM 'de' y",
    "EEEE, d 'de' MMMM 'de' y",
    "h:mm\u{a0}a",
    "HH:mm",
    "{1} {0}",
);
static PT_PT_DATES: DatePatterns = dates(
    "dd/MM/yy",
    "dd/MM/y",
    "d 'de' MMMM 'de' y",
    "EEEE, d 'de' MMMM 'de' y",
    "h:mm\u{a0}a",
    "HH:mm",
    "{1}, {0}",
);
static RU_DATES: DatePatterns = dates(
    "dd.MM.y",
    "d MMM y 'г'.",
    "d MMMM y 'г'.",
    "EEEE, d MMMM y 'г'.",
    "h:mm\u{a0}a",
    "HH:mm",
    "{1}, {0}",
);
static PL_DATES: DatePatterns = dates(
    "d.MM.y",
    "d MMM y",
    "d MMMM y",
    "EEEE, d MMMM y",
    "h:mm\u{a0}a",
    "HH:mm",
    "{1}, {0}",
);
static JA_DATES: DatePatterns = dates(
    "y/MM/dd",
    "y/MM/dd",
    "y年M月d日",
    "y年M月d日EEEE",
    "aK:mm",
    "H:mm",
    "{1} {0}",
);
static KO_DATES: DatePatterns = dates(
    "yy. M. d.",
    "y. M. d.",
    "y년 M월 d일",
    "y년 M월 d일 EEEE",
    "a h:mm",
    "H:mm",
    "{1} {0}",
);
static ZH_DATES: DatePatterns = dates(
    "y/M/d",
    "y年M月d日",
    "y年M月d日",
    "y年M月d日EEEE",
    "ah:mm",
    "HH:mm",
    "{1} {0}",
);
static ZH_HANT_DATES: DatePatterns = dates(
    "y/M/d",
    "y年M月d日",
    "y年M月d日",
    "y年M月d日 EEEE",
    "ah:mm",
    "HH:mm",
    "{1} {0}",
);
static TR_DATES: DatePatterns = dates(
    "d.MM.y",
    "d MMM y",
    "d MMMM y",
    "d MMMM y EEEE",
    "a h:mm",
    "HH:mm",
    "{1} {0}",
);
static AR_DATES: DatePatterns = dates(
    "d\u{200f}/M\u{200f}/y",
    "dd\u{200f}/MM\u{200f}/y",
    "d MMMM y",
    "EEEE، d MMMM y",
    "h:mm a",
    "HH:mm",
    "{1}، {0}",
);
static TH_DATES: DatePatterns = dates(
    "d/M/yy",
    "d MMM y",
    "d MMMM y",
    "EEEEที่ d MMMM y",
    "h:mm a",
    "HH:mm",
    "{1} {0}",
);
static VI_DATES: DatePatterns = dates(
    "dd/MM/y",
    "d MMM, y",
    "d MMMM, y",
    "EEEE, d MMMM, y",
    "h:mm a",
    "HH:mm",
    "{0} {1}",
);
static ID_DATES: DatePatterns = dates(
    "dd/MM/yy",
    "d MMM y",
    "d MMMM y",
    "EEEE, d MMMM y",
    "h.mm a",
    "HH.mm",
    "{1} {0}",
);
static NL_DATES: DatePatterns = dates(
    "dd-MM-y",
    "d MMM y",
    "d MMMM y",
    "EEEE d MMMM y",
    "h:mm a",
    "HH:mm",
    "{1} {0}",
);
static SV_DATES: DatePatterns = dates(
    "y-MM-dd",
    "d MMM y",
    "d MMMM y",
    "EEEE d MMMM y",
    "h:mm a",
    "HH:mm",
    "{1} {0}",
);
static DA_DATES: DatePatterns = dates(
    "dd.MM.y",
    "d. MMM y",
    "d. MMMM y",
    "EEEE 'den' d. MMMM y",
    "h.mm a",
    "HH.mm",
    "{1} {0}",
);
static NB_DATES: DatePatterns = dates(
    "dd.MM.y",
    "d. MMM y",
    "d. MMMM y",
    "EEEE d. MMMM y",
    "h:mm a",
    "HH:mm",
    "{1}, {0}",
);
static FI_DATES: DatePatterns = dates(
    "d.M.y",
    "d.M.y",
    "d. MMMM y",
    "EEEE d. MMMM y",
    "h.mm a",
    "H.mm",
    "{1} {0}",
);
static CS_DATES: DatePatterns = dates(
    "dd.MM.yy",
    "d. M. y",
    "d. MMMM y",
    "EEEE d. MMMM y",
    "h:mm a",
    "H:mm",
    "{1} {0}",
);
static HU_DATES: DatePatterns = dates(
    "y. MM. dd.",
    "y. MMM d.",
    "y. MMMM d.",
    "y. MMMM d., EEEE",
    "a h:mm",
    "H:mm",
    "{1} {0}",
);
static UK_DATES: DatePatterns = dates(
    "dd.MM.yy",
    "d MMM y 'р'.",
    "d MMMM y 'р'.",
    "EEEE, d MMMM y 'р'.",
    "h:mm a",
    "HH:mm",
    "{1}, {0}",
);
static EL_DATES: DatePatterns = dates(
    "d/M/yy",
    "d MMM y",
    "d MMMM y",
    "EEEE d MMMM y",
    "h:mm a",
    "HH:mm",
    "{1} - {0}",
);
static RO_DATES: DatePatterns = dates(
    "dd.MM.y",
    "d MMM y",
    "d MMMM y",
    "EEEE, d MMMM y",
    "h:mm a",
    "HH:mm",
    "{1}, {0}",
);
static HI_DATES: DatePatterns = dates(
    "d/M/yy",
    "d MMM y",
    "d MMMM y",
    "EEEE, d MMMM y",
    "h:mm a",
    "HH:mm",
    "{1}, {0}",
);

// --- Lists --------------------------------------------------------------------------------------

const fn list(and: &'static str, or: &'static str) -> ListData {
    ListData {
        and: ListPatterns {
            two: and,
            middle: ", ",
            end: and,
        },
        or: ListPatterns {
            two: or,
            middle: ", ",
            end: or,
        },
    }
}

pub(crate) static ROOT_LIST: ListData = list(", ", ", ");
static EN_LIST: ListData = ListData {
    and: ListPatterns {
        two: " and ",
        middle: ", ",
        end: ", and ",
    },
    or: ListPatterns {
        two: " or ",
        middle: ", ",
        end: ", or ",
    },
};
static EN_001_LIST: ListData = list(" and ", " or ");
static FR_LIST: ListData = list(" et ", " ou ");
static DE_LIST: ListData = list(" und ", " oder ");
static ES_LIST: ListData = list(" y ", " o ");
static IT_LIST: ListData = list(" e ", " o ");
static PT_LIST: ListData = list(" e ", " ou ");
static RU_LIST: ListData = list(" и ", " или ");
static PL_LIST: ListData = list(" i ", " lub ");
static NL_LIST: ListData = list(" en ", " of ");
static SV_LIST: ListData = list(" och ", " eller ");
static DA_LIST: ListData = list(" og ", " eller ");
static NB_LIST: ListData = list(" og ", " eller ");
static FI_LIST: ListData = list(" ja ", " tai ");
static CS_LIST: ListData = list(" a ", " nebo ");
static HU_LIST: ListData = list(" és ", " vagy ");
static UK_LIST: ListData = list(" і ", " або ");
static EL_LIST: ListData = list(" και ", " ή ");
static RO_LIST: ListData = list(" și ", " sau ");
static TR_LIST: ListData = list(" ve ", " veya ");
static ID_LIST: ListData = list(" dan ", " atau ");
static VI_LIST: ListData = list(" và ", " hoặc ");
static HI_LIST: ListData = ListData {
    and: ListPatterns {
        two: " और ",
        middle: ", ",
        end: ", और ",
    },
    or: ListPatterns {
        two: " या ",
        middle: ", ",
        end: " या ",
    },
};
static AR_LIST: ListData = ListData {
    and: ListPatterns {
        two: " و",
        middle: " و",
        end: " و",
    },
    or: ListPatterns {
        two: " أو ",
        middle: " أو ",
        end: " أو ",
    },
};
static TH_LIST: ListData = ListData {
    and: ListPatterns {
        two: "และ",
        middle: " ",
        end: " และ",
    },
    or: ListPatterns {
        two: " หรือ ",
        middle: " ",
        end: " หรือ ",
    },
};
static JA_LIST: ListData = ListData {
    and: ListPatterns {
        two: "、",
        middle: "、",
        end: "、",
    },
    or: ListPatterns {
        two: "または",
        middle: "、",
        end: "、または",
    },
};
static ZH_LIST: ListData = ListData {
    and: ListPatterns {
        two: "和",
        middle: "、",
        end: "和",
    },
    or: ListPatterns {
        two: "或",
        middle: "、",
        end: "或",
    },
};
static KO_LIST: ListData = ListData {
    and: ListPatterns {
        two: " 및 ",
        middle: ", ",
        end: " 및 ",
    },
    or: ListPatterns {
        two: " 또는 ",
        middle: ", ",
        end: " 또는 ",
    },
};

// --- Relative time ------------------------------------------------------------------------------

pub(crate) static ROOT_REL: RelativeData = RelativeData {
    future: "+{0} {1}",
    past: "-{0} {1}",
    units: [
        &[("other", "s")],
        &[("other", "min")],
        &[("other", "h")],
        &[("other", "d")],
        &[("other", "w")],
        &[("other", "m")],
        &[("other", "y")],
    ],
    days: None,
};
static EN_REL: RelativeData = RelativeData {
    future: "in {0} {1}",
    past: "{0} {1} ago",
    units: [
        &[("one", "second"), ("other", "seconds")],
        &[("one", "minute"), ("other", "minutes")],
        &[("one", "hour"), ("other", "hours")],
        &[("one", "day"), ("other", "days")],
        &[("one", "week"), ("other", "weeks")],
        &[("one", "month"), ("other", "months")],
        &[("one", "year"), ("other", "years")],
    ],
    days: Some(["yesterday", "today", "tomorrow"]),
};
static FR_REL: RelativeData = RelativeData {
    future: "dans {0} {1}",
    past: "il y a {0} {1}",
    units: [
        &[("one", "seconde"), ("other", "secondes")],
        &[("one", "minute"), ("other", "minutes")],
        &[("one", "heure"), ("other", "heures")],
        &[("one", "jour"), ("other", "jours")],
        &[("one", "semaine"), ("other", "semaines")],
        &[("one", "mois"), ("other", "mois")],
        &[("one", "an"), ("other", "ans")],
    ],
    days: Some(["hier", "aujourd’hui", "demain"]),
};
static DE_REL: RelativeData = RelativeData {
    future: "in {0} {1}",
    past: "vor {0} {1}",
    units: [
        &[("one", "Sekunde"), ("other", "Sekunden")],
        &[("one", "Minute"), ("other", "Minuten")],
        &[("one", "Stunde"), ("other", "Stunden")],
        &[("one", "Tag"), ("other", "Tagen")],
        &[("one", "Woche"), ("other", "Wochen")],
        &[("one", "Monat"), ("other", "Monaten")],
        &[("one", "Jahr"), ("other", "Jahren")],
    ],
    days: Some(["gestern", "heute", "morgen"]),
};
static ES_REL: RelativeData = RelativeData {
    future: "dentro de {0} {1}",
    past: "hace {0} {1}",
    units: [
        &[("one", "segundo"), ("other", "segundos")],
        &[("one", "minuto"), ("other", "minutos")],
        &[("one", "hora"), ("other", "horas")],
        &[("one", "día"), ("other", "días")],
        &[("one", "semana"), ("other", "semanas")],
        &[("one", "mes"), ("other", "meses")],
        &[("one", "año"), ("other", "años")],
    ],
    days: Some(["ayer", "hoy", "mañana"]),
};
static IT_REL: RelativeData = RelativeData {
    future: "tra {0} {1}",
    past: "{0} {1} fa",
    units: [
        &[("one", "secondo"), ("other", "secondi")],
        &[("one", "minuto"), ("other", "minuti")],
        &[("one", "ora"), ("other", "ore")],
        &[("one", "giorno"), ("other", "giorni")],
        &[("one", "settimana"), ("other", "settimane")],
        &[("one", "mese"), ("other", "mesi")],
        &[("one", "anno"), ("other", "anni")],
    ],
    days: Some(["ieri", "oggi", "domani"]),
};
static PT_REL: RelativeData = RelativeData {
    future: "em {0} {1}",
    past: "há {0} {1}",
    units: [
        &[("one", "segundo"), ("other", "segundos")],
        &[("one", "minuto"), ("other", "minutos")],
        &[("one", "hora"), ("other", "horas")],
        &[("one", "dia"), ("other", "dias")],
        &[("one", "semana"), ("other", "semanas")],
        &[("one", "mês"), ("other", "meses")],
        &[("one", "ano"), ("other", "anos")],
    ],
    days: Some(["ontem", "hoje", "amanhã"]),
};
static RU_REL: RelativeData = RelativeData {
    future: "через {0} {1}",
    past: "{0} {1} назад",
    units: [
        &[
            ("one", "секунду"),
            ("few", "секунды"),
            ("many", "секунд"),
            ("other", "секунды"),
        ],
        &[
            ("one", "минуту"),
            ("few", "минуты"),
            ("many", "минут"),
            ("other", "минуты"),
        ],
        &[
            ("one", "час"),
            ("few", "часа"),
            ("many", "часов"),
            ("other", "часа"),
        ],
        &[
            ("one", "день"),
            ("few", "дня"),
            ("many", "дней"),
            ("other", "дня"),
        ],
        &[
            ("one", "неделю"),
            ("few", "недели"),
            ("many", "недель"),
            ("other", "недели"),
        ],
        &[
            ("one", "месяц"),
            ("few", "месяца"),
            ("many", "месяцев"),
            ("other", "месяца"),
        ],
        &[
            ("one", "год"),
            ("few", "года"),
            ("many", "лет"),
            ("other", "года"),
        ],
    ],
    days: Some(["вчера", "сегодня", "завтра"]),
};
static PL_REL: RelativeData = RelativeData {
    future: "za {0} {1}",
    past: "{0} {1} temu",
    units: [
        &[
            ("one", "sekundę"),
            ("few", "sekundy"),
            ("many", "sekund"),
            ("other", "sekundy"),
        ],
        &[
            ("one", "minutę"),
            ("few", "minuty"),
            ("many", "minut"),
            ("other", "minuty"),
        ],
        &[
            ("one", "godzinę"),
            ("few", "godziny"),
            ("many", "godzin"),
            ("other", "godziny"),
        ],
        &[
            ("one", "dzień"),
            ("few", "dni"),
            ("many", "dni"),
            ("other", "dnia"),
        ],
        &[
            ("one", "tydzień"),
            ("few", "tygodnie"),
            ("many", "tygodni"),
            ("other", "tygodnia"),
        ],
        &[
            ("one", "miesiąc"),
            ("few", "miesiące"),
            ("many", "miesięcy"),
            ("other", "miesiąca"),
        ],
        &[
            ("one", "rok"),
            ("few", "lata"),
            ("many", "lat"),
            ("other", "roku"),
        ],
    ],
    days: Some(["wczoraj", "dzisiaj", "jutro"]),
};
static JA_REL: RelativeData = RelativeData {
    future: "{0} {1}後",
    past: "{0} {1}前",
    units: [
        &[("other", "秒")],
        &[("other", "分")],
        &[("other", "時間")],
        &[("other", "日")],
        &[("other", "週間")],
        &[("other", "か月")],
        &[("other", "年")],
    ],
    days: Some(["昨日", "今日", "明日"]),
};
static KO_REL: RelativeData = RelativeData {
    future: "{0}{1} 후",
    past: "{0}{1} 전",
    units: [
        &[("other", "초")],
        &[("other", "분")],
        &[("other", "시간")],
        &[("other", "일")],
        &[("other", "주")],
        &[("other", "개월")],
        &[("other", "년")],
    ],
    days: Some(["어제", "오늘", "내일"]),
};
static ZH_REL: RelativeData = RelativeData {
    future: "{0}{1}后",
    past: "{0}{1}前",
    units: [
        &[("other", "秒钟")],
        &[("other", "分钟")],
        &[("other", "小时")],
        &[("other", "天")],
        &[("other", "周")],
        &[("other", "个月")],
        &[("other", "年")],
    ],
    days: Some(["昨天", "今天", "明天"]),
};

// --- The table ----------------------------------------------------------------------------------

/// The locale table, sorted by tag (binary search). `root` is the last fallback of every chain.
pub(crate) static LOCALES: &[LocaleData] = &[
    LocaleData {
        tag: "ar",
        number: Some(&AR_NUM),
        native: Some(&ARAB_DIGITS),
        calendar: Some(&AR_CAL),
        dates: Some(&AR_DATES),
        hour12: Some(true),
        currencies: &[("SAR", "ر.س.\u{200f}"), ("USD", "US$")],
        list: Some(&AR_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "cs",
        number: Some(&CS_NUM),
        calendar: Some(&CS_CAL),
        dates: Some(&CS_DATES),
        hour12: Some(false),
        currencies: &[("CZK", "Kč"), ("USD", "US$")],
        list: Some(&CS_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "da",
        number: Some(&DA_NUM),
        calendar: Some(&DA_CAL),
        dates: Some(&DA_DATES),
        hour12: Some(false),
        currencies: &[("DKK", "kr."), ("USD", "US$")],
        list: Some(&DA_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "de",
        number: Some(&DE_NUM),
        calendar: Some(&DE_CAL),
        dates: Some(&DE_DATES),
        hour12: Some(false),
        currencies: &[("USD", "$"), ("JPY", "¥")],
        list: Some(&DE_LIST),
        relative: Some(&DE_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "el",
        number: Some(&EL_NUM),
        calendar: Some(&EL_CAL),
        dates: Some(&EL_DATES),
        hour12: Some(true),
        list: Some(&EL_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "en",
        number: Some(&EN_NUM),
        calendar: Some(&EN_CAL),
        dates: Some(&EN_DATES),
        hour12: Some(true),
        currencies: &[("USD", "$"), ("JPY", "¥")],
        list: Some(&EN_LIST),
        relative: Some(&EN_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "en-001",
        dates: Some(&EN_001_DATES),
        currencies: &[("USD", "US$")],
        list: Some(&EN_001_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "en-AU",
        parent: Some("en-001"),
        currencies: &[("AUD", "$")],
        ..EMPTY
    },
    LocaleData {
        tag: "en-CA",
        parent: Some("en-001"),
        currencies: &[("CAD", "$")],
        ..EMPTY
    },
    LocaleData {
        tag: "en-GB",
        parent: Some("en-001"),
        hour12: Some(false),
        currencies: &[("GBP", "£")],
        ..EMPTY
    },
    LocaleData {
        tag: "en-IN",
        parent: Some("en-001"),
        number: Some(&IN_NUM),
        currencies: &[("INR", "₹")],
        ..EMPTY
    },
    // The pseudo and key-display debug locales (`loc_pseudo`) format as English.
    LocaleData {
        tag: "en-XA",
        parent: Some("en"),
        ..EMPTY
    },
    LocaleData {
        tag: "en-XD",
        parent: Some("en"),
        ..EMPTY
    },
    LocaleData {
        tag: "es",
        number: Some(&ES_NUM),
        calendar: Some(&ES_CAL),
        dates: Some(&ES_DATES),
        hour12: Some(false),
        currencies: &[("USD", "US$"), ("JPY", "JPY")],
        list: Some(&ES_LIST),
        relative: Some(&ES_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "es-419",
        number: Some(&ES_419_NUM),
        currencies: &[("USD", "USD")],
        ..EMPTY
    },
    LocaleData {
        tag: "es-AR",
        parent: Some("es-419"),
        currencies: &[("ARS", "$")],
        ..EMPTY
    },
    LocaleData {
        tag: "es-CL",
        parent: Some("es-419"),
        currencies: &[("CLP", "$")],
        ..EMPTY
    },
    LocaleData {
        tag: "es-CO",
        parent: Some("es-419"),
        currencies: &[("COP", "$")],
        ..EMPTY
    },
    LocaleData {
        tag: "es-MX",
        parent: Some("es-419"),
        dates: Some(&ES_MX_DATES),
        currencies: &[("MXN", "$")],
        ..EMPTY
    },
    LocaleData {
        tag: "es-US",
        parent: Some("es-419"),
        hour12: Some(true),
        currencies: &[("USD", "$")],
        ..EMPTY
    },
    LocaleData {
        tag: "fi",
        number: Some(&NORDIC_MINUS_NUM),
        calendar: Some(&FI_CAL),
        dates: Some(&FI_DATES),
        hour12: Some(false),
        list: Some(&FI_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "fr",
        number: Some(&FR_NUM),
        calendar: Some(&FR_CAL),
        dates: Some(&FR_DATES),
        hour12: Some(false),
        currencies: &[("USD", "$US"), ("CAD", "$CA"), ("GBP", "£GB"), ("JPY", "JPY")],
        list: Some(&FR_LIST),
        relative: Some(&FR_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "fr-CA",
        number: Some(&FR_CA_NUM),
        dates: Some(&FR_CA_DATES),
        currencies: &[("CAD", "$"), ("USD", "$\u{a0}US")],
        ..EMPTY
    },
    LocaleData {
        tag: "hi",
        number: Some(&IN_NUM),
        native: Some(&DEVA_DIGITS),
        calendar: Some(&HI_CAL),
        dates: Some(&HI_DATES),
        hour12: Some(true),
        currencies: &[("INR", "₹"), ("USD", "$")],
        list: Some(&HI_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "hu",
        number: Some(&SPACE_GROUP_NUM),
        calendar: Some(&HU_CAL),
        dates: Some(&HU_DATES),
        hour12: Some(false),
        currencies: &[("HUF", "Ft"), ("USD", "USD")],
        list: Some(&HU_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "id",
        number: Some(&ID_NUM),
        calendar: Some(&ID_CAL),
        dates: Some(&ID_DATES),
        hour12: Some(false),
        currencies: &[("IDR", "Rp"), ("USD", "US$")],
        list: Some(&ID_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "it",
        number: Some(&IT_NUM),
        calendar: Some(&IT_CAL),
        dates: Some(&IT_DATES),
        hour12: Some(false),
        currencies: &[("USD", "USD"), ("JPY", "JPY")],
        list: Some(&IT_LIST),
        relative: Some(&IT_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "ja",
        number: Some(&EN_NUM),
        calendar: Some(&JA_CAL),
        dates: Some(&JA_DATES),
        hour12: Some(false),
        currencies: &[("JPY", "￥"), ("USD", "$"), ("CNY", "元")],
        list: Some(&JA_LIST),
        relative: Some(&JA_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "ko",
        number: Some(&EN_NUM),
        calendar: Some(&KO_CAL),
        dates: Some(&KO_DATES),
        hour12: Some(true),
        currencies: &[("KRW", "₩"), ("USD", "US$")],
        list: Some(&KO_LIST),
        relative: Some(&KO_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "nb",
        number: Some(&NORDIC_MINUS_NUM),
        calendar: Some(&NB_CAL),
        dates: Some(&NB_DATES),
        hour12: Some(false),
        currencies: &[("NOK", "kr"), ("USD", "USD")],
        list: Some(&NB_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "nl",
        number: Some(&NL_NUM),
        calendar: Some(&NL_CAL),
        dates: Some(&NL_DATES),
        hour12: Some(false),
        currencies: &[("USD", "US$")],
        list: Some(&NL_LIST),
        ..EMPTY
    },
    // Norwegian without a script/variant reads as Bokmål.
    LocaleData {
        tag: "no",
        parent: Some("nb"),
        ..EMPTY
    },
    LocaleData {
        tag: "pl",
        number: Some(&PL_NUM),
        calendar: Some(&PL_CAL),
        dates: Some(&PL_DATES),
        hour12: Some(false),
        currencies: &[("PLN", "zł"), ("USD", "USD")],
        list: Some(&PL_LIST),
        relative: Some(&PL_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "pt",
        number: Some(&PT_NUM),
        calendar: Some(&PT_CAL),
        dates: Some(&PT_DATES),
        hour12: Some(false),
        currencies: &[("BRL", "R$"), ("USD", "US$"), ("JPY", "JP¥")],
        list: Some(&PT_LIST),
        relative: Some(&PT_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "pt-PT",
        number: Some(&PT_PT_NUM),
        dates: Some(&PT_PT_DATES),
        ..EMPTY
    },
    LocaleData {
        tag: "ro",
        number: Some(&RO_NUM),
        calendar: Some(&RO_CAL),
        dates: Some(&RO_DATES),
        hour12: Some(false),
        currencies: &[("RON", "RON"), ("USD", "USD")],
        list: Some(&RO_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "root",
        number: Some(&ROOT_NUM),
        calendar: Some(&ROOT_CAL),
        dates: Some(&ROOT_DATES),
        hour12: Some(false),
        list: Some(&ROOT_LIST),
        relative: Some(&ROOT_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "ru",
        number: Some(&RU_NUM),
        calendar: Some(&RU_CAL),
        dates: Some(&RU_DATES),
        hour12: Some(false),
        currencies: &[("RUB", "₽"), ("USD", "$"), ("UAH", "₴")],
        list: Some(&RU_LIST),
        relative: Some(&RU_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "sv",
        number: Some(&NORDIC_MINUS_NUM),
        calendar: Some(&SV_CAL),
        dates: Some(&SV_DATES),
        hour12: Some(false),
        currencies: &[("SEK", "kr"), ("USD", "US$")],
        list: Some(&SV_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "th",
        number: Some(&EN_NUM),
        native: Some(&THAI_DIGITS),
        calendar: Some(&TH_CAL),
        dates: Some(&TH_DATES),
        hour12: Some(false),
        currencies: &[("THB", "฿"), ("USD", "US$")],
        list: Some(&TH_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "tr",
        number: Some(&TR_NUM),
        calendar: Some(&TR_CAL),
        dates: Some(&TR_DATES),
        hour12: Some(false),
        currencies: &[("TRY", "₺"), ("USD", "$")],
        list: Some(&TR_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "uk",
        number: Some(&SPACE_GROUP_NUM),
        calendar: Some(&UK_CAL),
        dates: Some(&UK_DATES),
        hour12: Some(false),
        currencies: &[("UAH", "₴"), ("USD", "USD")],
        list: Some(&UK_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "vi",
        number: Some(&VI_NUM),
        calendar: Some(&VI_CAL),
        dates: Some(&VI_DATES),
        hour12: Some(false),
        currencies: &[("VND", "₫"), ("USD", "US$")],
        list: Some(&VI_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "zh",
        number: Some(&EN_NUM),
        calendar: Some(&ZH_CAL),
        dates: Some(&ZH_DATES),
        hour12: Some(false),
        currencies: &[("CNY", "¥"), ("USD", "US$"), ("JPY", "JP¥")],
        list: Some(&ZH_LIST),
        relative: Some(&ZH_REL),
        ..EMPTY
    },
    LocaleData {
        tag: "zh-HK",
        parent: Some("zh-Hant"),
        currencies: &[("HKD", "HK$")],
        ..EMPTY
    },
    // Traditional Chinese has `root` as its CLDR parent, not `zh`.
    LocaleData {
        tag: "zh-Hant",
        parent: Some("root"),
        number: Some(&EN_NUM),
        calendar: Some(&ZH_HANT_CAL),
        dates: Some(&ZH_HANT_DATES),
        hour12: Some(true),
        currencies: &[("TWD", "$"), ("USD", "US$")],
        list: Some(&ZH_LIST),
        ..EMPTY
    },
    LocaleData {
        tag: "zh-MO",
        parent: Some("zh-Hant"),
        ..EMPTY
    },
    LocaleData {
        tag: "zh-TW",
        parent: Some("zh-Hant"),
        ..EMPTY
    },
];
