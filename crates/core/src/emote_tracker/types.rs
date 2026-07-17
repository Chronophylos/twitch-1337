#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetEventKind {
    Bootstrap,
    Add,
    Remove,
    Rename,
    Gap,
}

impl SetEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Rename => "rename",
            Self::Gap => "gap",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bootstrap" => Some(Self::Bootstrap),
            "add" => Some(Self::Add),
            "remove" => Some(Self::Remove),
            "rename" => Some(Self::Rename),
            "gap" => Some(Self::Gap),
            _ => None,
        }
    }
}
