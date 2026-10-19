-- Invoice billing: bill this account by emailed Stripe invoice instead of
-- charging a card.
--
-- Enterprise customers with PO processing can't pay by card on demand. For
-- them, top-ups and auto top-ups create a Stripe invoice with
-- `collection_method = send_invoice`; Stripe emails it and handles the payment
-- link, reminders and receipts. Credits land when the invoice is paid, not
-- when it is issued.
--
-- Off by default and enabled by us, not self-service: it hands out credit on
-- terms, so it needs a human to approve the account first. Organizations are
-- rows in `users` too, so this covers billing an org as well as an individual.
ALTER TABLE users
    ADD COLUMN invoicing_enabled BOOLEAN NOT NULL DEFAULT false;

COMMENT ON COLUMN users.invoicing_enabled IS
    'Bill this account by emailed Stripe invoice rather than an immediate card charge. Enabled manually after approval; see payment_providers::PaymentProvider::create_and_send_invoice.';
