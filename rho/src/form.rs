//! Forms: questions an extension asks the user in one dialog
//! (`Context::ask`).

use std::collections::BTreeMap;

crate::id_type! {
    /// Names a `Field` in `Answers`.
    pub FieldKey
}

crate::id_type! {
    /// A `Choice` as returned in `Answers`.
    pub ChoiceValue
}

/// Questions to show the user in one dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    /// Shown above the fields.
    pub message: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// Names the field in `Answers`.
    pub key: FieldKey,
    pub title: String,
    pub description: Option<String>,
    pub kind: FieldKind,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    /// Free text.
    Text,
    Choice {
        options: Vec<Choice>,
        /// More than one option can be picked.
        multiple: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    /// What is returned in `Answers`.
    pub value: ChoiceValue,
    pub title: String,
    pub description: Option<String>,
}

/// Field key -> answer. One value for text and single choices; fields the
/// user left empty are missing.
pub type Answers = BTreeMap<FieldKey, Vec<String>>;

#[derive(Clone, Debug, PartialEq)]
pub enum FormReply {
    Answers(Answers),
    /// The user dismissed the form.
    Declined,
    /// The frontend cannot show forms.
    Unsupported,
}
