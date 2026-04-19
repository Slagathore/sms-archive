CREATE INDEX IF NOT EXISTS idx_attachments_gps_checked ON attachments(gps_checked);
CREATE INDEX IF NOT EXISTS idx_attachments_gps_coords ON attachments(gps_lat, gps_lon);
