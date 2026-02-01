-- Migration: Add max_instances column to bon_drivers table
-- Purpose: Enable concurrent usage control for BonDrivers
-- Date: 2026-01-22

-- Step 1: Add max_instances column with default value for existing records
ALTER TABLE bon_drivers ADD COLUMN max_instances INTEGER DEFAULT 1;

-- Step 2: Update existing records to ensure max_instances is NOT NULL
UPDATE bon_drivers SET max_instances = 1 WHERE max_instances IS NULL;

-- Step 3: Change column to NOT NULL (optional, for data integrity)
-- NOTE: This step should only be run if you're certain all records have max_instances set
-- ALTER TABLE bon_drivers ALTER COLUMN max_instances SET NOT NULL;

-- Verification query to check the new column
SELECT id, dll_path, max_instances FROM bon_drivers;
