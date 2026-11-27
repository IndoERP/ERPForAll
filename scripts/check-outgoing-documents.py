#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Send two synthetic documents from a local test ERP to two independently configured Mailpits."""
import argparse
import base64
import json
import secrets
import subprocess
import time
from datetime import date
from email import policy
from email.parser import BytesParser
from http.cookiejar import CookieJar
from pathlib import Path
from urllib import error, parse, request


def loopback(value):
    uri = parse.urlsplit(value)
    if uri.hostname not in {"localhost", "127.0.0.1", "::1"} or uri.scheme not in {"http", "https"} or uri.username or uri.password:
        raise argparse.ArgumentTypeError("use a loopback HTTP(S) URL for the test fixture")
    return value.rstrip("/")


class Session:
    def __init__(self, origin, database, login, password):
        self.origin = origin
        self.browser = request.build_opener(request.HTTPCookieProcessor(CookieJar()))
        page = self.browser.open(origin + "/erp/login", timeout=10).read().decode()
        model = json.loads(page.split('id="erp-login">', 1)[1].split("</script>", 1)[0])
        form = parse.urlencode(dict(db=database, login=login, password=password, csrf_token=model["csrf"]))
        self.browser.open(origin + "/erp/login", form.encode(), timeout=15).read()

    def call(self, company, model, method, **params):
        params.setdefault("context", {})["allowed_company_ids"] = [company]
        req = request.Request(f"{self.origin}/erp/json/2/{model}/{method}", data=json.dumps(params).encode(),
                              headers={"Content-Type": "application/json", "Sec-Fetch-Site": "same-origin", "Origin": self.origin})
        try:
            return json.load(self.browser.open(req, timeout=30))
        except error.HTTPError as exc:
            # Server response bodies may contain business data or fixture credentials.
            raise RuntimeError(f"{model}.{method} returned HTTP {exc.code}") from None

    def create(self, company, model, **vals):
        record, = self.call(company, model, "create", vals_list=[vals])
        return record


def messages(mailpit, subject):
    query = parse.urlencode({"query": "subject:" + subject})
    with request.urlopen(f"{mailpit}/api/v1/search?{query}", timeout=8) as resp:
        return [msg for msg in json.load(resp)["messages"] if msg["Subject"] == subject]


def exercise(erp, company, mailbox, other_box, invoice):
    tag = secrets.token_hex(6)
    address = f"client-{tag}@example.test"
    subject = f"ERP-document-{tag}"
    customer = erp.create(company, "res.partner", name=f"Customer {tag}", email=address, company_id=company)
    today = date.today().isoformat()
    if invoice:
        model, wizard, field = "account.move", "account.invoice.email", "move_id"
        opener, sender = "action_open_invoice_email", "action_send_email"
        doc = erp.create(company, model, move_type="out_invoice", partner_id=customer, date=today, invoice_date=today,
                         invoice_line_ids=[[0, 0, dict(name="Service " + tag, quantity=1, price_unit=43)]])
        erp.call(company, model, "action_post", ids=[doc])
    else:
        model, wizard, field = "sale.order", "sale.quotation.send", "order_id"
        opener, sender = "action_open_send_wizard", "action_send_mail"
        doc = erp.create(company, model, company_id=company, partner_id=customer, partner_invoice_id=customer,
                         partner_shipping_id=customer, date_order=today + " 10:00:00", client_order_ref=tag)
    action = erp.call(company, model, opener, ids=[doc])
    assert action["res_model"] == wizard
    draft = erp.create(company, wizard, **{field: doc}, email_to=address, subject=subject, body="Approved " + tag)
    preview = erp.call(company, wizard, "action_preview_pdf", ids=[draft])
    row, = erp.call(company, "mail.document.preview", "read", ids=[preview["res_id"]], fields=["content"],
                    context={"bin_size": False})
    approved = base64.b64decode(row["content"], validate=True)
    assert approved.startswith(b"%PDF")
    if not invoice:
        erp.call(company, model, "write", ids=[doc], vals={"client_order_ref": "after-preview-" + tag})
    erp.call(company, wizard, sender, ids=[draft])
    domain = [["subject", "=", subject]]
    outgoing, = erp.call(company, "mail.outgoing", "search", domain=domain)
    routes = erp.call(company, "mail.smtp.relay", "search", domain=[["company_id", "=", company]])
    route, = erp.call(company, "mail.smtp.relay", "read", ids=routes, fields=["sender"])

    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        matches = messages(mailbox, subject)
        if matches:
            break
        state, = erp.call(company, "mail.outgoing", "read", ids=[outgoing], fields=["state"])
        if state["state"] in {"failed", "uncertain"}:
            raise RuntimeError(f"{model} delivery {outgoing} ended in {state['state']}")
        time.sleep(0.5)
    else:
        raise RuntimeError(f"Timed out waiting for {model} delivery {outgoing}")
    match, = matches
    assert not messages(other_box, subject)
    with request.urlopen(f"{mailbox}/api/v1/message/{match['ID']}/raw", timeout=8) as resp:
        message = BytesParser(policy=policy.default).parsebytes(resp.read())
    assert message["From"].addresses[0].addr_spec == route["sender"]
    assert message["To"].addresses[0].addr_spec == address
    assert tag in message.get_body(preferencelist=("plain",)).get_content()
    pdf, = message.iter_attachments()
    assert pdf.get_content_type() == "application/pdf" and pdf.get_payload(decode=True) == approved
    rendered = subprocess.run(["pdftotext", "-", "-"], check=True, input=approved, stdout=subprocess.PIPE).stdout.decode()
    assert tag in rendered and "after-preview-" + tag not in rendered
    print(f"Verified {model} {doc}, company {company}, {len(approved)} approved PDF bytes")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--erp", required=True, type=loopback)
    p.add_argument("--database", required=True)
    p.add_argument("--login", required=True)
    p.add_argument("--password-file", required=True, type=Path)
    for kind in ("quotation", "invoice"):
        p.add_argument(f"--{kind}-company", required=True, type=int)
        p.add_argument(f"--{kind}-mailpit", required=True, type=loopback)
    args = p.parse_args()
    if args.invoice_company == args.quotation_company or args.invoice_mailpit == args.quotation_mailpit:
        p.error("the fixture needs two distinct companies and two Mailpit instances")
    session = Session(args.erp, args.database, args.login, args.password_file.read_text().strip())
    exercise(session, args.quotation_company, args.quotation_mailpit, args.invoice_mailpit, False)
    exercise(session, args.invoice_company, args.invoice_mailpit, args.quotation_mailpit, True)


if __name__ == "__main__":
    main()
