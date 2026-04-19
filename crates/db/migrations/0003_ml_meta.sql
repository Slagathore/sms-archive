-- db/migrations/0003_ml_meta.sql

ALTER TABLE ml_models ADD COLUMN dims INTEGER;
ALTER TABLE ml_models ADD COLUMN max_length INTEGER;
ALTER TABLE ml_models ADD COLUMN normalize INTEGER;
ALTER TABLE ml_models ADD COLUMN tokenizer_path TEXT;
ALTER TABLE ml_models ADD COLUMN input_ids_name TEXT;
ALTER TABLE ml_models ADD COLUMN attention_mask_name TEXT;
ALTER TABLE ml_models ADD COLUMN token_type_ids_name TEXT;
ALTER TABLE ml_models ADD COLUMN output_name TEXT;

UPDATE schema_version SET version = 3 WHERE version < 3;
