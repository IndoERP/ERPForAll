// SPDX-License-Identifier: MIT OR Apache-2.0
//! PDF/A-3 e-invoice container: the structured invoice XML is embedded as an associated file and
//! the Factur-X / ZUGFeRD XMP properties with their PDF/A extension schema are added, all in one
//! incremental update.
use crate::attach::{Duplicate, EmbeddedFileOptions, Relationship, embed};
use crate::edit::{Update, caller_text};
use crate::error::{PdfError, Result, guard, invalid, refused};
use crate::limits::Context;
use crate::meta::{MetadataUpdate, apply};
use crate::xmp::{self, BagItem, Description, Packet};
use crate::{Emitter, Limit, PdfDate, PdfErrorKind, PdfLimits};

const FX: &str = "urn:factur-x:pdfa:CrossIndustryDocument:invoice:1p0#";
const CII: &str = "urn:un:unece:uncefact:data:standard:CrossIndustryInvoice:100";
const RAM: &str =
    "urn:un:unece:uncefact:data:standard:ReusableAggregateBusinessInformationEntity:100";
const PROPERTY: &str = "http://www.aiim.org/pdfa/ns/property#";

/// Factur-X / ZUGFeRD profile, taken from the XML guideline identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConformanceLevel {
    /// `MINIMUM`.
    Minimum,
    /// `BASIC WL`.
    BasicWl,
    /// `BASIC`.
    Basic,
    /// `EN 16931` (formerly COMFORT).
    En16931,
    /// `EXTENDED`.
    Extended,
    /// `XRECHNUNG`.
    XRechnung,
}
impl ConformanceLevel {
    /// Value written to `fx:ConformanceLevel`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimum => "MINIMUM",
            Self::BasicWl => "BASIC WL",
            Self::Basic => "BASIC",
            Self::En16931 => "EN 16931",
            Self::Extended => "EXTENDED",
            Self::XRechnung => "XRECHNUNG",
        }
    }
    /// MINIMUM and BASIC WL carry data only; richer profiles are an alternative representation.
    fn relationship(self) -> Relationship {
        match self {
            Self::Minimum | Self::BasicWl => Relationship::Data,
            _ => Relationship::Alternative,
        }
    }
    fn from_guideline(id: &str) -> Option<Self> {
        Some(match id {
            "urn:factur-x.eu:1p0:minimum" => Self::Minimum,
            "urn:factur-x.eu:1p0:basicwl" => Self::BasicWl,
            "urn:cen.eu:en16931:2017#compliant#urn:factur-x.eu:1p0:basic" => Self::Basic,
            "urn:cen.eu:en16931:2017" => Self::En16931,
            "urn:cen.eu:en16931:2017#conformant#urn:factur-x.eu:1p0:extended" => Self::Extended,
            x if x.starts_with(
                "urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_",
            ) =>
            {
                Self::XRechnung
            }
            _ => return None,
        })
    }
}
/// Name of the embedded invoice file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EInvoiceName {
    /// `factur-x.xml`.
    FacturX,
    /// `xrechnung.xml` (required for the XRECHNUNG profile).
    XRechnung,
}
impl EInvoiceName {
    fn file(self) -> &'static str {
        match self {
            Self::FacturX => "factur-x.xml",
            Self::XRechnung => "xrechnung.xml",
        }
    }
}
/// Validated container options; description and date are mandatory for PDF/A-3 files.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EInvoiceOptions {
    name: EInvoiceName,
    description: String,
    date: PdfDate,
}
impl EInvoiceOptions {
    /// `date` is the file's modification date; the library reads no clock.
    pub fn new(
        name: EInvoiceName,
        description: &str,
        date: PdfDate,
        limits: &PdfLimits,
    ) -> Result<Self> {
        caller_text("e-invoice description", description, limits)?;
        if description.trim().is_empty() {
            return Err(invalid("PDF e-invoice description is empty"));
        }
        Ok(Self {
            name,
            description: description.into(),
            date,
        })
    }
}
/// Profile of a UN/CEFACT Cross Industry Invoice from its guideline identifier.
pub fn einvoice_level(xml: &[u8], limits: &PdfLimits) -> Result<ConformanceLevel> {
    guard(|| level(xml, &Context::new(limits)))
}
fn level(xml: &[u8], l: &Context<'_>) -> Result<ConformanceLevel> {
    l.check(Limit::DecodedBytes, xml.len())?;
    let text = std::str::from_utf8(xml.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(xml))
        .map_err(|e| invalid("PDF e-invoice XML must be UTF-8").source(e))?;
    let doc = roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: u32::try_from(l.get(Limit::WorkUnits)).unwrap_or(u32::MAX),
            ..Default::default()
        },
    )
    .map_err(|e| match e {
        roxmltree::Error::DtdDetected => refused("e-invoice XML DTD/entity").source(e),
        _ => invalid("PDF e-invoice XML is malformed").source(e),
    })?;
    let root = doc.root_element();
    if root.tag_name().namespace() != Some(CII) || root.tag_name().name() != "CrossIndustryInvoice"
    {
        return Err(invalid(
            "PDF e-invoice XML must be a UN/CEFACT CrossIndustryInvoice",
        ));
    }
    fn child<'a, 'i>(
        n: roxmltree::Node<'a, 'i>,
        ns: &str,
        local: &str,
    ) -> Option<roxmltree::Node<'a, 'i>> {
        n.children().find(|c| {
            c.is_element() && c.tag_name().namespace() == Some(ns) && c.tag_name().name() == local
        })
    }
    let id = child(root, CII, "ExchangedDocumentContext")
        .and_then(|c| child(c, RAM, "GuidelineSpecifiedDocumentContextParameter"))
        .and_then(|c| child(c, RAM, "ID"))
        .and_then(|c| c.text())
        .map(str::trim)
        .ok_or_else(|| invalid("PDF e-invoice XML lacks its guideline identifier"))?;
    ConformanceLevel::from_guideline(id).ok_or_else(|| {
        invalid(format!(
            "PDF e-invoice guideline {id:?} is not a known profile"
        ))
    })
}
fn property(name: &str, kind: &str, about: &str) -> String {
    format!(
        "<rdf:li rdf:parseType=\"Resource\"><pdfaProperty:name>{name}</pdfaProperty:name>\
         <pdfaProperty:valueType>{kind}</pdfaProperty:valueType>\
         <pdfaProperty:category>external</pdfaProperty:category>\
         <pdfaProperty:description>{about}</pdfaProperty:description></rdf:li>"
    )
}
/// Turn a PDF/A-3 document into an e-invoice container as one incremental update.
///
/// The input must declare `pdfaid:part` 3; its `pdfaid` level, Info and other XMP properties are
/// kept, so Info and XMP stay equal. `fx:ConformanceLevel` and `AFRelationship` follow the XML
/// profile. The Factur-X extension schema joins the input's `pdfaExtension:schemas` bag when it
/// has one, whose entries stay byte for byte. Encrypted and signed inputs and existing Factur-X
/// data are refused. Structural success is not a veraPDF or Factur-X validator verdict. Discard
/// all emitted bytes on error.
pub fn einvoice(
    input: &[u8],
    xml: &[u8],
    options: &EInvoiceOptions,
    limits: &PdfLimits,
    emit: &mut Emitter<'_>,
) -> Result<ConformanceLevel> {
    einvoice_with_policy(
        input,
        xml,
        options,
        limits,
        emit,
        crate::SignedEditPolicy::Refuse,
    )
    .map(|(level, _)| level)
}
pub(crate) fn einvoice_with_policy(
    input: &[u8],
    xml: &[u8],
    options: &EInvoiceOptions,
    limits: &PdfLimits,
    emit: &mut Emitter<'_>,
    policy: crate::SignedEditPolicy,
) -> Result<(ConformanceLevel, crate::IncrementalEditSummary)> {
    guard(|| {
        let context = Context::new(limits);
        let l = &context;
        let level = level(xml, l)?;
        if (level == ConformanceLevel::XRechnung) != (options.name == EInvoiceName::XRechnung) {
            return Err(invalid(format!(
                "PDF e-invoice profile {} requires {}",
                level.as_str(),
                if level == ConformanceLevel::XRechnung {
                    "xrechnung.xml"
                } else {
                    "factur-x.xml"
                }
            )));
        }
        let mut u = Update::load(input, l)?;
        let summary = u.admit(policy, input.len())?;
        let packet = u
            .xmp(l)?
            .ok_or_else(|| conformance("input has no XMP metadata"))?;
        let doc = xmp::parse(&packet, l)?;
        let packet_view = Packet::new(&doc, l)?;
        match packet_view.pdfa()? {
            Some(id) if id.part == 3 => {}
            Some(id) => {
                return Err(conformance(&format!(
                    "input is PDF/A-{}, not PDF/A-3",
                    id.part
                )));
            }
            None => return Err(conformance("input declares no PDF/A part")),
        }
        if packet_view.has_namespace(FX) || packet_view.declares_schema(FX)? {
            return Err(invalid("PDF input already carries Factur-X metadata"));
        }
        drop(doc);
        let file = options.name.file();
        let attachment =
            EmbeddedFileOptions::new(file, "text/xml", level.relationship(), options.date, limits)?
                .description(&options.description, limits)?;
        embed(&mut u, xml, &attachment, Duplicate::Refuse, l)?;
        let fx = Description {
            ns: &[("fx", FX)],
            body: format!(
                "<fx:DocumentType>INVOICE</fx:DocumentType><fx:DocumentFileName>{file}</fx:DocumentFileName>\
                 <fx:Version>1.0</fx:Version><fx:ConformanceLevel>{}</fx:ConformanceLevel>",
                level.as_str()
            ),
        };
        // One schema entry; it declares its own prefixes so it is valid in any existing bag.
        let entry = |declarations: &str| {
            format!(
                "<rdf:li{declarations} rdf:parseType=\"Resource\">\
                 <pdfaSchema:schema>Factur-X PDFA Extension Schema</pdfaSchema:schema>\
                 <pdfaSchema:namespaceURI>{FX}</pdfaSchema:namespaceURI>\
                 <pdfaSchema:prefix>fx</pdfaSchema:prefix>\
                 <pdfaSchema:property><rdf:Seq>{}{}{}{}</rdf:Seq></pdfaSchema:property>\
                 </rdf:li>",
                property(
                    "DocumentFileName",
                    "Text",
                    "File name of the embedded invoice XML"
                ),
                property("DocumentType", "Text", "Kind of embedded document"),
                property("Version", "Text", "Version of the invoice XML schema"),
                property(
                    "ConformanceLevel",
                    "Text",
                    "Profile of the embedded invoice XML"
                ),
            )
        };
        let schema = BagItem {
            ns: xmp::EXTENSION,
            local: "schemas",
            item: entry(&format!(
                " xmlns:rdf=\"{}\" xmlns:pdfaSchema=\"{}\" xmlns:pdfaProperty=\"{PROPERTY}\"",
                xmp::RDF,
                xmp::SCHEMA
            )),
            absent: Description {
                ns: &[
                    ("pdfaExtension", xmp::EXTENSION),
                    ("pdfaSchema", xmp::SCHEMA),
                    ("pdfaProperty", PROPERTY),
                ],
                body: format!(
                    "<pdfaExtension:schemas><rdf:Bag>{}</rdf:Bag></pdfaExtension:schemas>",
                    entry("")
                ),
            },
        };
        apply(&mut u, &MetadataUpdate::new(), &[fx], Some(&schema), l)?;
        u.save(input, l, emit)?;
        Ok((level, summary))
    })
}
fn conformance(what: &str) -> PdfError {
    PdfError::new(
        PdfErrorKind::Unsupported,
        format!("PDF e-invoice container requires PDF/A-3: {what} (a later release)"),
    )
}
