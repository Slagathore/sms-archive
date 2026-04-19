-- Mission: extend message storage to track direction (incoming/outgoing) so UI can distinguish sent vs received messages.

ALTER TABLE messages ADD COLUMN message_direction INTEGER DEFAULT 0;
