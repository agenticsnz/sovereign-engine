-- Companion mmproj (multimodal projector) GGUF filename, sibling of the
-- primary model file in the same hf_repo directory. NULL for text-only
-- models. Path stored as "<filename>" (no directory prefix), mirroring
-- the existing `filename` column convention.
ALTER TABLE models ADD COLUMN mmproj_filename TEXT;
