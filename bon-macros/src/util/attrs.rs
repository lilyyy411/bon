pub(crate) trait AttributeExt {
    fn is_doc(&self) -> bool;
    fn as_doc(&self) -> Option<&syn::Expr>;
}

impl AttributeExt for syn::Attribute {
    fn is_doc(&self) -> bool {
        self.as_doc().is_some()
    }

    fn as_doc(&self) -> Option<&syn::Expr> {
        let attr = match &self.meta {
            syn::Meta::NameValue(attr) => attr,
            _ => return None,
        };

        if !attr.path.is_ident("doc") {
            return None;
        }

        Some(&attr.value)
    }
}
