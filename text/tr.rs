use clap::Parser;
use deunicode::deunicode_char;
use gettextrs::{bind_textdomain_codeset, setlocale, textdomain, LocaleCategory};
use plib::PROJECT_NAME;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::{self, Read, Write};
use std::iter::{self, Peekable};
use std::process;
use std::slice::Iter;
use std::str::Chars;

/// tr - translate or delete characters
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Delete characters in STRING1 from the input
    #[arg(short = 'd')]
    delete: bool,

    /// Replace each input sequence of a repeated character that is listed in the last specified SET, with a single occurrence of that character
    #[arg(short = 's')]
    squeeze_repeats: bool,

    /// Use the complement of STRING1's values
    #[arg(short = 'c')]
    complement_val: bool,

    /// Use the complement of STRING1's characters
    #[arg(short = 'C')]
    complement_char: bool,

    /// First string
    string1: String,

    /// Second string (not required if delete mode is on)
    string2: Option<String>,
}

impl Args {
    fn validate_args(&self) -> Result<(), String> {
        // Check if conflicting options are used together
        if self.complement_char && self.complement_val {
            return Err("Options '-c' and '-C' cannot be used together".to_owned());
        }

        if self.squeeze_repeats
            && self.string2.is_none()
            && (self.complement_char || self.complement_val)
            && !self.delete
        {
            return Err("Option '-c' or '-C' may only be used with 2 strings".to_owned());
        }

        if !self.squeeze_repeats && !self.delete && self.string2.is_none() {
            return Err("Need two strings operand".to_owned());
        }

        if self.string1.is_empty() {
            return Err("At least 1 string operand is required".to_owned());
        }

        Ok(())
    }
}

#[derive(Clone)]
enum CharRepetition {
    AsManyAsNeeded,
    N(usize),
}

#[derive(Clone)]
enum Operand {
    Char {
        // The character
        char: char,
        // The number of times the character is repeated
        char_repetition: CharRepetition,
    },
    Equiv {
        // The character equivalent
        char: char,
    },
}

impl Operand {
    fn char(&self) -> &char {
        match self {
            Self::Char { char, .. } => char,
            Self::Equiv { char } => char,
        }
    }
}

impl Operand {
    /// Checks if a target character exists in a vector of `Operand` elements.
    ///
    /// # Arguments
    ///
    /// * `operands` - A reference to a vector of `Operand` elements.
    /// * `target` - A reference to the target character to search for.
    ///
    /// # Returns
    ///
    /// `true` if the target character is found, `false` otherwise.
    fn contains(operands: &[Operand], target: &char) -> bool {
        for operand in operands {
            match operand {
                Operand::Equiv { char } => {
                    if compare_deunicoded_chars(*char, *target) {
                        return true;
                    }
                }
                Operand::Char { char, .. } => {
                    if char == target {
                        return true;
                    }
                }
            }
        }

        false
    }
}

/// Parses a sequence in the format `[=equiv=]` from the given character iterator.
///
/// The function expects the iterator to be positioned just before the first `=`
/// character. It reads the equivalent characters between the `=` symbols and
/// creates a list of `Operand::Equiv` entries, one for each character.
///
/// # Arguments
///
/// * `chars` - A mutable reference to a peekable character iterator.
///
/// # Returns
///
/// A `Result` containing a vector of `Operand::Equiv` entries if successful, or a
/// `String` describing the error if parsing fails.
///
/// # Errors
///
/// This function will return an error if:
/// - The sequence does not contain a closing `=` before `]`.
/// - The sequence does not contain a closing `]`.
/// - The sequence contains no characters between the `=` symbols.
///
fn parse_equiv(square_bracket_constructs_buffer: &[char]) -> Result<Operand, String> {
    let mut iter = square_bracket_constructs_buffer.iter();

    // Skip '[='
    assert!(iter.next() == Some(&'['));
    assert!(iter.next() == Some(&'='));

    let mut between_equals_signs = Vec::<char>::with_capacity(1_usize);

    between_equals_signs.extend(iter.take_while(|&&ch| ch != '='));

    let char_between_equals_signs = match between_equals_signs.as_slice() {
        &[ch] => ch,
        &[] => {
            unreachable!();
        }
        sl => {
            const ERROR_MESSAGE_PREFIX: &str = "tr: ";
            const ERROR_MESSAGE_SUFFIX: &str =
                ": equivalence class operand must be a single character";

            const ERROR_MESSAGE_PREFIX_AND_SUFFIX_LENGTH: usize =
                ERROR_MESSAGE_PREFIX.len() + ERROR_MESSAGE_SUFFIX.len();

            let mut error_message =
                String::with_capacity(ERROR_MESSAGE_PREFIX_AND_SUFFIX_LENGTH + sl.len());

            error_message.push_str(ERROR_MESSAGE_PREFIX);

            for &ch in sl {
                error_message.push(ch);
            }

            error_message.push_str(ERROR_MESSAGE_SUFFIX);

            return Err(error_message);
        }
    };

    let operand = Operand::Equiv {
        char: char_between_equals_signs,
    };

    Ok(operand)
}

fn parse_repeated_char(square_bracket_constructs_buffer: &[char]) -> Result<Operand, String> {
    // TODO
    // Clean this up
    fn fill_repeat_str(iter: &mut Iter<char>, repeat_string: &mut String) {
        while let Some(&ch) = iter.next() {
            if ch == ']' {
                assert!(iter.next().is_none());

                return;
            }

            repeat_string.push(ch);
        }

        unreachable!();
    }

    let mut iter = square_bracket_constructs_buffer.iter();

    // Skip '['
    assert!(iter.next() == Some(&'['));

    // Get character before '*'
    let char = iter.next().unwrap().to_owned();

    // Skip '*'
    assert!(iter.next() == Some(&'*'));

    let mut repeat_string = String::with_capacity(square_bracket_constructs_buffer.len());

    fill_repeat_str(&mut iter, &mut repeat_string);

    // "If n is omitted or is zero, it shall be interpreted as large enough to extend the string2-based sequence to the length of the string1-based sequence. If n has a leading zero, it shall be interpreted as an octal value. Otherwise, it shall be interpreted as a decimal value."
    // https://pubs.opengroup.org/onlinepubs/9799919799/utilities/tr.html
    let char_repetition = match repeat_string.as_str() {
        "" => CharRepetition::AsManyAsNeeded,
        st => {
            let radix = if st.starts_with('0') {
                // Octal
                8_u32
            } else {
                10_u32
            };

            match usize::from_str_radix(st, radix) {
                Ok(0_usize) => CharRepetition::AsManyAsNeeded,
                Ok(n) => CharRepetition::N(n),
                Err(_pa) => {
                    return Err(format!(
                        "tr: invalid repeat count ‘{st}’ in [c*n] construct",
                    ));
                }
            }
        }
    };

    let operand = Operand::Char {
        char,
        char_repetition,
    };

    Ok(operand)
}

fn parse_octal_sequence(
    first_octal_digit: char,
    peekable: &mut Peekable<Chars>,
) -> Result<char, String> {
    let mut st = String::with_capacity(3_usize);

    st.push(first_octal_digit);

    let mut added_octal_digit_to_buffer = |pe: &mut Peekable<Chars>| {
        if let Some(&second_octal_digit @ '0'..='7') = pe.peek() {
            st.push(second_octal_digit);

            true
        } else {
            false
        }
    };

    let advance_peekable_if_parsing_succeed = if added_octal_digit_to_buffer(peekable) {
        peekable.next();

        added_octal_digit_to_buffer(peekable)
    } else {
        false
    };

    let from_str_radix_result = u16::from_str_radix(&st, 8_u32);

    let octal_digits_parsed = match from_str_radix_result {
        Ok(uo) => uo,
        Err(pa) => {
            return Err(format!("tr: failed to parse octal sequence '{st}' ({pa})"));
        }
    };

    // There is no consensus on how to handle this:
    //
    // BusyBox and GNU Core Utilities:
    //     parse "\501" as \050 (which is '(') and '1'
    //         GNU Core Utilities prints a warning, BusyBox does not
    // uutils' coreutils:
    //     parses "\501" as '1'
    // bsdutils:
    //     parses "\501" as 'Ł' (U+0141)
    //
    // None of these implementations treat this as a fatal error
    // POSIX says: "Multi-byte characters require multiple, concatenated escape sequences of this type, including the leading <backslash> for each byte."
    //
    // Following BusyBox and GNU Core Utilities, because their handling seems to be most in keeping with the POSIX
    // specification
    let byte = match u8::try_from(octal_digits_parsed) {
        Ok(ue) => {
            if advance_peekable_if_parsing_succeed {
                peekable.next();
            }

            ue
        }
        Err(_tr) => {
            // This should only happen when the sequence is \400 and above
            // Cannot happen with a two character sequence like \77, because 8^2 is 64 (within u8 bounds)
            assert!(st.len() == 3_usize);

            let mut chars = st.chars();

            let third_octal_digit = chars.next_back().unwrap();

            // `chars_str` is a view of the first two octal digits
            let chars_str = chars.as_str();

            assert!(chars_str.len() == 2_usize);

            // Treat the sequence \abc (where a, b, and c are octal digits) as \0abc
            // The byte represented by \0ab is what will be returned from this function
            // Parsing of c is handled outside this function
            match u8::from_str_radix(chars_str, 8_u32) {
                Ok(ue) => {
                    eprintln!(
                        "tr: warning: the ambiguous octal escape \\{st} is being interpreted as the 2-byte sequence \\0{chars_str}, {third_octal_digit}"
                    );

                    ue
                }
                Err(pa) => {
                    return Err(format!("tr: invalid octal sequence '{chars_str}' ({pa})"));
                }
            }
        }
    };

    let char = char::from(byte);

    Ok(char)
}

fn parse_single_char(peekable: &mut Peekable<Chars>) -> Result<Option<char>, String> {
    let option = match peekable.next() {
        Some('\\') => {
            let char = match peekable.next() {
                /* #region \octal */
                Some(first_octal_digit @ '0'..='7') => {
                    parse_octal_sequence(first_octal_digit, peekable)?
                }
                /* #endregion */
                //
                /* #region \character */
                // <alert>
                // Code point 0007
                Some('a') => '\u{0007}',
                // <backspace>
                // Code point 0008
                Some('b') => '\u{0008}',
                // <tab>
                // Code point 0009
                Some('t') => '\u{0009}',
                // <newline>
                // Code point 000A
                Some('n') => '\u{000A}',
                // <vertical-tab>
                // Code point 000B
                Some('v') => '\u{000B}',
                // <form-feed>
                // Code point 000C
                Some('f') => '\u{000C}',
                // <carriage-return>
                // Code point 000D
                Some('r') => '\u{000D}',
                // <backslash>
                // Code point 005C
                Some('\\') => {
                    // An escaped backslash
                    '\u{005C}'
                }
                /* #endregion */
                //
                Some(cha) => {
                    // If a backslash is not at the end of the string, and is not followed by one of the valid
                    // escape characters (including another backslash), the backslash is basically just ignored:
                    // the following character is the character added to the set.
                    cha
                }
                None => {
                    eprintln!(
                        "tr: warning: an unescaped backslash at end of string is not portable"
                    );

                    // If an unescaped backslash is the last character of the string, treat it as though it were
                    // escaped (backslash is added to the set)
                    '\u{005C}'
                }
            };

            Some(char)
        }
        op => op,
    };

    Ok(option)
}

fn parse_range_or_single_char(
    starting_char: char,
    peekable: &mut Peekable<Chars>,
    operand_vec: &mut Vec<Operand>,
) -> Result<(), String> {
    match peekable.peek() {
        Some(&hyphen @ '-') => {
            // Possible "c-c" construct
            // Move past `hyphen`
            peekable.next();

            // The parsed character after the hyphen
            // e.g. "tr 'A-Z' '\044-1'"
            match parse_single_char(peekable)? {
                Some(after_hyphen) => {
                    // Ranges are inclusive
                    let range_inclusive = starting_char..=after_hyphen;

                    if range_inclusive.is_empty() {
                        let message =
                            format!(
                                "tr: range-endpoints of '{}-{}' are in reverse collating sequence order",
                                starting_char.escape_default(),
                                after_hyphen.escape_default()
                            );

                        return Err(message);
                    }

                    let range_inclusive_to_operand_iterator =
                        range_inclusive.map(|ch| Operand::Char {
                            char: ch,
                            char_repetition: CharRepetition::N(1_usize),
                        });

                    // TODO
                    // Does this reserve the right capacity?
                    operand_vec.extend(range_inclusive_to_operand_iterator);
                }
                None => {
                    // End of input, do not handle as a range
                    // e.g. "tr 'ab' 'c-'"
                    operand_vec.extend_from_slice(&[
                        Operand::Char {
                            char: starting_char,
                            char_repetition: CharRepetition::N(1_usize),
                        },
                        Operand::Char {
                            char: hyphen,
                            char_repetition: CharRepetition::N(1_usize),
                        },
                    ]);
                }
            }
        }
        _ => {
            // Not a "c-c" construct
            operand_vec.push(Operand::Char {
                char: starting_char,
                char_repetition: CharRepetition::N(1_usize),
            })
        }
    }

    Ok(())
}

fn parse_string1_or_string2(string1_or_string2: &str) -> Result<Vec<Operand>, String> {
    // The longest valid "[:class:]", "[=equiv=]", or "[x*n]" construct is a "[x*n]" construct
    // These are (seemingly) the shortest invalid "[x*n]" constructs (octal and decimal):
    // [a*010000000000000000000000]
    // [a*100000000000000000000]
    // Therefore, the longest valid one should be:
    // [a*01000000000000000000000]
    // Rounding up to 32
    const SQUARE_BRACKET_CONSTRUCTS_BUFFER_CAPACITY: usize = 32_usize;

    // This capacity will be sufficient at least some of the time
    let mut operand_vec = Vec::<Operand>::with_capacity(string1_or_string2.len());

    let mut peekable = string1_or_string2.chars().peekable();

    let mut parse_left_square_bracket_normally = false;

    while let Some(&ch) = peekable.peek() {
        match (ch, parse_left_square_bracket_normally) {
            ('[', false) => {
                // Save the state of `peekable` before advancing it, see note below
                let peekable_saved = peekable.clone();

                // TODO
                // Avoid repeated allocation
                let mut square_bracket_constructs_buffer =
                    Vec::<char>::with_capacity(SQUARE_BRACKET_CONSTRUCTS_BUFFER_CAPACITY);

                let mut found_closing_square_bracket = false;

                for ch in peekable.by_ref() {
                    square_bracket_constructs_buffer.push(ch);

                    let vec_len = square_bracket_constructs_buffer.len();

                    // Length check is a hacky fix for "[:]", "[=]", "[]*]", etc.
                    if ch == ']' && vec_len > 3_usize {
                        found_closing_square_bracket = true;

                        break;
                    }
                }

                if found_closing_square_bracket {
                    let after_opening_square_bracket =
                        square_bracket_constructs_buffer.get(1_usize);

                    let before_closing_square_bracket =
                        square_bracket_constructs_buffer.iter().rev().nth(1_usize);

                    if after_opening_square_bracket == Some(&':')
                        && before_closing_square_bracket == Some(&':')
                    {
                        expand_character_class(
                            &square_bracket_constructs_buffer,
                            &mut operand_vec,
                        )?;

                        continue;
                    }

                    if after_opening_square_bracket == Some(&'=')
                        && before_closing_square_bracket == Some(&'=')
                    {
                        // "[=equiv=]" construct
                        let operand = parse_equiv(&square_bracket_constructs_buffer)?;

                        operand_vec.push(operand);

                        continue;
                    }

                    if square_bracket_constructs_buffer.get(2_usize) == Some(&'*') {
                        // "[x*n]" construct
                        operand_vec.push(parse_repeated_char(&square_bracket_constructs_buffer)?);

                        continue;
                    }
                }

                // Not a "[:class:]", "[=equiv=]", or "[x*n]" construct
                // The hacky way to continue is to reset `peekable` (to `peekable_saved`)
                // This moves the Peekable back to the point it was at before attempting to parse square bracket
                // constructs
                parse_left_square_bracket_normally = true;
                peekable = peekable_saved;
            }
            (cha, bo) => {
                // '[' is not the start of a square bracket construct, so handle it normally
                if bo {
                    assert!(cha == '[');

                    // When encountering '[' in the future, try to parse square bracket constructs first
                    parse_left_square_bracket_normally = false;
                }

                if let Some(char) = parse_single_char(&mut peekable)? {
                    parse_range_or_single_char(char, &mut peekable, &mut operand_vec)?;
                }
            }
        }
    }

    Ok(operand_vec)
}

/// Compares two characters after normalizing them.
/// This function uses the hypothetical `deunicode_char` function to normalize
/// the input characters and then compares them for equality.
/// # Arguments
///
/// * `char1` - The first character to compare.
/// * `char2` - The second character to compare.
///
/// # Returns
///
/// * `true` if the normalized characters are equal.
/// * `false` otherwise.
fn compare_deunicoded_chars(char1: char, char2: char) -> bool {
    let normalized_char1 = deunicode_char(char1);
    let normalized_char2 = deunicode_char(char2);

    normalized_char1 == normalized_char2
}

fn expand_character_class(
    square_bracket_constructs_buffer: &[char],
    operand_vec: &mut Vec<Operand>,
) -> Result<(), String> {
    // "[:class:]" construct
    let mut into_iter = square_bracket_constructs_buffer.iter();

    assert!(into_iter.next() == Some(&'['));
    assert!(into_iter.next() == Some(&':'));

    assert!(into_iter.next_back() == Some(&']'));
    assert!(into_iter.next_back() == Some(&':'));

    // TODO
    // Performance
    let class = into_iter.collect::<String>();

    let char_vec = match class.as_str() {
        "alnum" => ('0'..='9')
            .chain('A'..='Z')
            .chain('a'..='z')
            .collect::<Vec<_>>(),
        "alpha" => ('A'..='Z').chain('a'..='z').collect::<Vec<_>>(),
        "digit" => ('0'..='9').collect::<Vec<_>>(),
        "lower" => ('a'..='z').collect::<Vec<_>>(),
        "upper" => ('A'..='Z').collect::<Vec<_>>(),
        "space" => vec![' ', '\t', '\n', '\r', '\x0b', '\x0c'],
        "blank" => vec![' ', '\t'],
        "cntrl" => (0..=31)
            .chain(iter::once(127))
            .map(|it| char::from(it as u8))
            .collect::<Vec<_>>(),
        "graph" => (33..=126)
            .map(|it| char::from(it as u8))
            .collect::<Vec<_>>(),
        "print" => (32..=126)
            .map(|it| char::from(it as u8))
            .collect::<Vec<_>>(),
        "punct" => (33..=47)
            .chain(58..=64)
            .chain(91..=96)
            .chain(123..=126)
            .map(|it| char::from(it as u8))
            .collect::<Vec<_>>(),
        "xdigit" => ('0'..='9')
            .chain('A'..='F')
            .chain('a'..='f')
            .collect::<Vec<_>>(),
        _ => return Err("Error: Invalid class name ".to_owned()),
    };

    operand_vec.reserve(char_vec.len());

    for ch in char_vec {
        operand_vec.push(Operand::Char {
            char: ch,
            char_repetition: CharRepetition::N(1_usize),
        });
    }

    Ok(())
}

/// Computes the complement of a string with respect to two sets of characters.
///
/// This function takes an input string and two sets of characters (`chars1` and `chars2`)
/// and computes the complement of the input string with respect to the characters in `chars1`.
/// For each character in the input string:
/// - If the character is present in `chars1`, it remains unchanged in the result.
/// - If the character is not present in `chars1`, it is replaced with characters from `chars2`
///   sequentially until all characters in `chars2` are exhausted, and then the process repeats.
///
/// # Arguments
///
/// * `input` - A string slice representing the input string.
/// * `chars1` - A vector of `Operand` representing the first set of characters.
/// * `chars2` - A vector of `Operand` representing the second set of characters.
///
/// # Returns
///
/// * `String` - Returns a string representing the complement of the input string.
///
fn complement_chars(
    input: &str,
    chars1: &[Operand],
    chars2: &[Operand],
) -> Result<String, Box<dyn Error>> {
    let mut depleted = Vec::<usize>::with_capacity(chars2.len());

    for op in chars2 {
        let us = match op {
            Operand::Char {
                char_repetition: CharRepetition::N(n),
                ..
            } => *n,
            _ => usize::MAX,
        };

        depleted.push(us);
    }

    let depleted_clone = depleted.clone();

    // Create a variable to store the result
    let mut result = String::new();

    // Initialize the index for the chars2 vector
    let mut chars2_index = 0;

    // Go through each character in the input string
    for ch in input.chars() {
        // Check if the character is in the chars1 vector
        if Operand::contains(chars1, &ch) {
            // If the character is in the chars1 vector, add it to the result without changing it
            result.push(ch);

            continue;
        }

        // If the character is not in the chars1 vector, replace it with a character from the chars2 vector
        // Add the character from the chars2 vector to the result
        // TODO
        // Indexing
        let operand = chars2.get(chars2_index).ok_or("Indexing failed")?;

        match operand {
            Operand::Char { char, .. } => {
                result.push(*char);

                let mut_ref = depleted.get_mut(chars2_index).ok_or("Indexing failed")?;

                let decremented = (*mut_ref) - 1_usize;

                *mut_ref = decremented;

                if decremented > 0_usize {
                    continue;
                }
            }
            Operand::Equiv { char } => {
                result.push(*char);
            }
        }

        // Increase the index for the chars2 vector
        chars2_index += 1;

        // If the index has reached the end of the chars2 vector, reset it to zero
        if chars2_index >= chars2.len() {
            chars2_index = 0;

            depleted.clone_from(&depleted_clone);
        }
    }

    Ok(result)
}

/// Checks if a character is repeatable based on certain conditions.
///
/// This function determines if a character `c` is repeatable based on the following conditions:
/// - The character occurs more than once in the input string.
/// - The character is present in the set `set2`.
///
/// If the conditions are met and the character has not been seen before, it is considered repeatable.
///
/// # Arguments
///
/// * `c` - A character to be checked for repeatability.
/// * `char_counts` - A reference to a hashmap containing character counts in the input string.
/// * `seen` - A mutable reference to a hash set to keep track of characters seen so far.
/// * `set2` - A reference to a vector of `Operand` representing the second set of characters.
///
/// # Returns
///
/// * `bool` - Returns `true` if the character is repeatable based on the conditions specified above.
///            Returns `false` otherwise.
///
fn check_repeatable(
    ch: char,
    char_counts: &HashMap<char, usize>,
    seen: &mut HashSet<char>,
    set2: &[Operand],
) -> bool {
    if char_counts[&ch] > 1 && Operand::contains(set2, &ch) {
        if seen.contains(&ch) {
            false
        } else {
            seen.insert(ch);

            true
        }
    } else {
        true
    }
}

// TODO
// This should be optimized
fn generate_transformation_hash_map(
    string1_operands: &[Operand],
    string2_operands: &[Operand],
) -> Result<HashMap<char, char>, Box<dyn Error>> {
    let mut char_repeating_total = 0_usize;

    let mut string1_operands_flattened = Vec::<char>::new();

    for op in string1_operands {
        match op {
            Operand::Char {
                char_repetition,
                char,
            } => match char_repetition {
                CharRepetition::AsManyAsNeeded => {
                    return Err(Box::from(
                        "tr: the [c*] repeat construct may not appear in string1".to_owned(),
                    ));
                }
                CharRepetition::N(n) => {
                    let n_usize = *n;

                    char_repeating_total = char_repeating_total
                        .checked_add(n_usize)
                        .ok_or("Arithmetic overflow")?;

                    for _ in 0_usize..n_usize {
                        string1_operands_flattened.push(*char);
                    }
                }
            },
            _ => {
                return Err(Box::from("Expectation violated".to_owned()));
            }
        }
    }

    let mut as_many_as_needed_index = Option::<usize>::None;

    let mut replacement_char_repeating_total = 0_usize;

    for (us, op) in string2_operands.iter().enumerate() {
        match op {
            Operand::Char {
                char_repetition, ..
            } => match char_repetition {
                CharRepetition::AsManyAsNeeded => {
                    if as_many_as_needed_index.is_some() {
                        return Err(Box::from(
                            "tr: only one [c*] repeat construct may appear in string2".to_owned(),
                        ));
                    }

                    as_many_as_needed_index = Some(us);
                }
                CharRepetition::N(n) => {
                    replacement_char_repeating_total = replacement_char_repeating_total
                        .checked_add(*n)
                        .ok_or("Arithmetic overflow")?;
                }
            },
            _ => {
                return Err(Box::from("Expectation violated".to_owned()));
            }
        }
    }

    let mut string2_operands_with_leftover: Vec<Operand>;

    let string2_operands_to_use = if replacement_char_repeating_total < char_repeating_total {
        let leftover = char_repeating_total
            .checked_sub(replacement_char_repeating_total)
            .ok_or("Arithmetic overflow")?;

        string2_operands_with_leftover = string2_operands.to_vec();

        match as_many_as_needed_index {
            Some(us) => {
                let op = string2_operands_with_leftover
                    .get_mut(us)
                    .ok_or("Indexing failed")?;

                match op {
                    Operand::Char { char, .. } => {
                        *op = Operand::Char {
                            char: *char,
                            char_repetition: CharRepetition::N(leftover),
                        };
                    }
                    _ => {
                        return Err(Box::from("Expectation violated".to_owned()));
                    }
                }
            }
            None => {
                let op = string2_operands_with_leftover
                    .last_mut()
                    .ok_or("Unexpected empty collection")?;

                match op {
                    Operand::Char {
                        char,
                        char_repetition,
                    } => {
                        let current_n = match char_repetition {
                            CharRepetition::N(n) => n,
                            _ => {
                                return Err(Box::from("Expectation violated".to_owned()));
                            }
                        };

                        let current_n_plus_leftover = current_n
                            .checked_add(leftover)
                            .ok_or("Arithmetic overflow")?;

                        *op = Operand::Char {
                            char: *char,
                            char_repetition: CharRepetition::N(current_n_plus_leftover),
                        };
                    }
                    _ => {
                        return Err(Box::from("Expectation violated".to_owned()));
                    }
                }
            }
        }

        &string2_operands_with_leftover
    } else {
        string2_operands
    };

    // TODO
    // Capacity
    let mut string2_operands_to_use_flattened = Vec::<char>::new();

    for op in string2_operands_to_use {
        match op {
            Operand::Char {
                char_repetition,
                char,
            } => match char_repetition {
                CharRepetition::N(n) => {
                    for _ in 0_usize..(*n) {
                        string2_operands_to_use_flattened.push(*char);
                    }
                }
                CharRepetition::AsManyAsNeeded => {
                    // The "[c*]" construct was not needed, ignore it
                }
            },
            _ => {
                return Err(Box::from("Expectation violated".to_owned()));
            }
        }
    }

    let mut translation_hash_map = HashMap::<char, char>::new();

    for (us, ch) in string1_operands_flattened.into_iter().enumerate() {
        let cha = string2_operands_to_use_flattened
            .get(us)
            .ok_or("Indexing failed")?;

        translation_hash_map.insert(ch, *cha);
    }

    Ok(translation_hash_map)
}

/// Translates or deletes characters from standard input, according to specified arguments.
///
/// This function reads from standard input, processes the input string based on the specified arguments,
/// and prints the result to standard output. It supports translation of characters, deletion of characters,
/// and squeezing repeated characters.
///
/// # Arguments
///
/// * `args` - A reference to an `Args` struct containing the command-line arguments.
///
/// # Returns
///
/// * `Result<(), Box<dyn std::error::Error>>` - Returns `Ok(())` on success. Returns an error wrapped in `Box<dyn std::error::Error>`
///   if there is an error reading from standard input or processing the input string.
///
fn tr(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let string1_operands = parse_string1_or_string2(&args.string1)?;

    let string2_operands_option = match &args.string2 {
        Some(st) => Some(parse_string1_or_string2(st)?),
        None => None,
    };

    let mut input = String::new();

    // TODO
    // tr should be streaming
    io::stdin()
        .read_to_string(&mut input)
        .expect("Failed to read input");

    let mut stdout_lock = io::stdout().lock();

    if args.delete {
        let filtered_string = if args.complement_char || args.complement_val {
            input
                .chars()
                .filter(|c| Operand::contains(&string1_operands, c))
                .collect::<String>()
        } else {
            input
                .chars()
                .filter(|c| !Operand::contains(&string1_operands, c))
                .collect::<String>()
        };

        let filtered_string_to_use = if args.squeeze_repeats && string2_operands_option.is_some() {
            // Counting the frequency of characters in the chars vector
            let mut char_counts = HashMap::<char, usize>::new();

            for ch in filtered_string.chars() {
                *(char_counts.entry(ch).or_insert(0)) += 1;
            }

            let mut seen = HashSet::<char>::new();

            filtered_string
                .chars()
                .filter(|&ch| {
                    check_repeatable(
                        ch,
                        &char_counts,
                        &mut seen,
                        string2_operands_option.as_deref().unwrap(),
                    )
                })
                .collect::<String>()
        } else {
            filtered_string
        };

        stdout_lock.write_all(filtered_string_to_use.as_bytes())?;

        Ok(())
    } else if args.squeeze_repeats && string2_operands_option.is_none() {
        let mut char_counts = HashMap::<char, i32>::new();

        for ch in input.chars() {
            *(char_counts.entry(ch).or_insert(0)) += 1;
        }

        let mut seen = HashSet::<char>::new();

        let filtered_string = input
            .chars()
            .filter(|&ch| {
                if char_counts[&ch] > 1 && Operand::contains(&string1_operands, &ch) {
                    if seen.contains(&ch) {
                        false
                    } else {
                        seen.insert(ch);

                        true
                    }
                } else {
                    true
                }
            })
            .collect::<String>();

        stdout_lock.write_all(filtered_string.as_bytes())?;

        return Ok(());
    } else {
        let result_string = if args.complement_char || args.complement_val {
            if args.complement_char {
                complement_chars(
                    &input,
                    &string1_operands,
                    string2_operands_option.as_deref().unwrap(),
                )?
            } else {
                let mut set2 = string2_operands_option.as_deref().unwrap().to_vec();

                set2.sort_by(|op, ope| op.char().cmp(ope.char()));

                complement_chars(&input, &string1_operands, &set2)?
            }
        } else {
            let string2_operands = match string2_operands_option.as_deref() {
                Some(op) => op,
                None => {
                    return Err(Box::from("tr: missing operand".to_owned()));
                }
            };

            if string2_operands.is_empty() {
                return Err(Box::from(
                    "tr: when not truncating set1, string2 must be non-empty".to_owned(),
                ));
            }

            let transformation_map =
                generate_transformation_hash_map(&string1_operands, string2_operands)?;

            let mut result = String::with_capacity(input.len());

            for ch in input.chars() {
                let char_to_use = match transformation_map.get(&ch) {
                    Some(cha) => *cha,
                    None => ch,
                };

                result.push(char_to_use);
            }

            result
        };

        let result_string_to_use = if args.squeeze_repeats {
            // Counting the frequency of characters in the chars vector
            let mut char_counts = HashMap::<char, usize>::new();

            for ch in result_string.chars() {
                *(char_counts.entry(ch).or_insert(0)) += 1;
            }

            let mut seen = HashSet::<char>::new();

            result_string
                .chars()
                .filter(|&ch| {
                    check_repeatable(
                        ch,
                        &char_counts,
                        &mut seen,
                        string2_operands_option.as_deref().unwrap(),
                    )
                })
                .collect::<String>()
        } else {
            result_string
        };

        stdout_lock.write_all(result_string_to_use.as_bytes())?;

        return Ok(());
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    setlocale(LocaleCategory::LcAll, "");
    textdomain(PROJECT_NAME)?;
    bind_textdomain_codeset(PROJECT_NAME, "UTF-8")?;

    let args = Args::parse();

    args.validate_args()?;

    if let Err(err) = tr(&args) {
        eprintln!("{}", err);

        process::exit(1);
    }

    Ok(())
}
