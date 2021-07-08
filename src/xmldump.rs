use xml::reader::XmlEvent;
pub enum Page {
None,
Open,
Title(Vec<String>),
Titled(String),
Text { title: String, text: Vec<String> },
Texted { title: String, text: String },
}

impl Page {
pub fn parse(page: Self, event: XmlEvent) -> Self {
    match (page, event) {
        (Page::None, XmlEvent::StartElement { name, .. }) if name.local_name == "page" => {
            Page::Open
        }

        (Page::Open, XmlEvent::StartElement { name, .. }) if name.local_name == "title" => {
            Page::Title(Vec::with_capacity(1))
        }

        (Page::Title(mut ts), XmlEvent::Characters(s))
        | (Page::Title(mut ts), XmlEvent::CData(s)) => {
            ts.push(s);
            Page::Title(ts)
        }

        (Page::Title(ts), XmlEvent::EndElement { name }) if name.local_name == "title" => {
            Page::Titled(ts.join(" "))
        }

        (Page::Titled(title), XmlEvent::StartElement { name, .. })
            if name.local_name == "text" =>
        {
            Page::Text {
                title,
                text: Vec::with_capacity(5),
            }
        }

        (Page::Text { title, mut text }, XmlEvent::Characters(s))
        | (Page::Text { title, mut text }, XmlEvent::CData(s)) => {
            text.push(s);
            Page::Text { title, text }
        }

        (Page::Text { title, text }, XmlEvent::EndElement { name })
            if name.local_name == "text" =>
        {
            Page::Texted {
                title,
                text: text.join(" "),
            }
        }

        (Page::Texted { .. }, _) => Page::None,
        (_, XmlEvent::EndElement { name }) if name.local_name == "page" => Page::None,

        (p, _) => p,
    }
}
}
