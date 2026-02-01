-- Migration: Fill band_type and terrestrial_region for existing channels
--
-- This migration updates all rows in the channels table that have NULL values
-- for band_type and/or terrestrial_region by inferring them from the NID.
--
-- NID ranges:
-- - 0x7FXX: Terrestrial (地上波)
-- - 0x0004, 0x0005, 0x4001-0x400F: BS
-- - 0x0006, 0x0007, 0x000A, 0x6001-0x600F: CS
-- - 0x7C00-0x7CFF: 4K/UHD

-- Update band_type for all channels with NULL band_type
UPDATE channels
SET band_type = CASE
    WHEN nid = 4 OR nid = 5 OR (nid >= 0x4001 AND nid <= 0x400F) THEN 1  -- BS
    WHEN nid IN (6, 7, 10) OR (nid >= 0x6001 AND nid <= 0x600F) THEN 2   -- CS
    WHEN nid >= 0x7C00 AND nid <= 0x7CFF THEN 3                          -- 4K
    WHEN nid >= 0x7F00 AND nid <= 0x7FFF THEN 0                          -- Terrestrial
    ELSE 4                                                                 -- Other
END
WHERE band_type IS NULL;

-- Update terrestrial_region for terrestrial channels with NULL region
-- Prefecture-specific NIDs
UPDATE channels
SET terrestrial_region = CASE
    -- Hokkaido
    WHEN nid IN (0x7F01, 0x7FE0, 0x7FF0) THEN '北海道'
    
    -- Tohoku
    WHEN nid = 0x7F08 THEN '青森'
    WHEN nid = 0x7F09 THEN '岩手'
    WHEN nid = 0x7F0A THEN '宮城'
    WHEN nid = 0x7F0B THEN '秋田'
    WHEN nid = 0x7F0C THEN '山形'
    WHEN nid = 0x7F0D THEN '福島'
    
    -- Kanto prefectural
    WHEN nid = 0x7F0E THEN '茨城'
    WHEN nid = 0x7F0F THEN '栃木'
    WHEN nid = 0x7F10 THEN '群馬'
    WHEN nid = 0x7F11 THEN '埼玉'
    WHEN nid = 0x7F12 THEN '千葉'
    WHEN nid = 0x7F13 THEN '東京'
    WHEN nid = 0x7F14 THEN '神奈川'
    
    -- Koshinetsu
    WHEN nid = 0x7F15 THEN '新潟'
    WHEN nid = 0x7F16 THEN '長野'
    WHEN nid = 0x7F17 THEN '山梨'
    
    -- Hokuriku
    WHEN nid = 0x7F18 THEN '富山'
    WHEN nid = 0x7F19 THEN '石川'
    WHEN nid = 0x7F1A THEN '福井'
    
    -- Tokai prefectural
    WHEN nid = 0x7F1B THEN '静岡'
    WHEN nid = 0x7F1C THEN '愛知'
    WHEN nid = 0x7F1D THEN '岐阜'
    WHEN nid = 0x7F1E THEN '三重'
    
    -- Kinki prefectural
    WHEN nid = 0x7F1F THEN '滋賀'
    WHEN nid = 0x7F20 THEN '京都'
    WHEN nid = 0x7F21 THEN '大阪'
    WHEN nid = 0x7F22 THEN '兵庫'
    WHEN nid = 0x7F23 THEN '奈良'
    WHEN nid = 0x7F24 THEN '和歌山'
    
    -- Chugoku
    WHEN nid = 0x7F25 THEN '鳥取'
    WHEN nid = 0x7F26 THEN '島根'
    WHEN nid = 0x7F27 THEN '岡山'
    WHEN nid = 0x7F28 THEN '広島'
    WHEN nid = 0x7F29 THEN '山口'
    
    -- Shikoku
    WHEN nid = 0x7F2A THEN '徳島'
    WHEN nid = 0x7F2B THEN '香川'
    WHEN nid = 0x7F2C THEN '愛媛'
    WHEN nid = 0x7F2D THEN '高知'
    
    -- Kyushu
    WHEN nid = 0x7F2E THEN '福岡'
    WHEN nid = 0x7F2F THEN '佐賀'
    WHEN nid = 0x7F30 THEN '長崎'
    WHEN nid = 0x7F31 THEN '熊本'
    WHEN nid = 0x7F32 THEN '大分'
    WHEN nid = 0x7F33 THEN '宮崎'
    WHEN nid = 0x7F34 THEN '鹿児島'
    
    -- Okinawa
    WHEN nid = 0x7F35 THEN '沖縄'
    
    -- Wide area broadcast NIDs
    WHEN nid >= 0x7FE0 AND nid <= 0x7FE7 THEN '北海道'
    WHEN nid = 0x7FE8 THEN '東京'
    WHEN nid = 0x7FE9 THEN '大阪'
    WHEN nid = 0x7FEA THEN '愛知'
    WHEN nid = 0x7FEB THEN '岡山'
    WHEN nid = 0x7FEC THEN '島根'
    WHEN nid >= 0x7FF0 AND nid <= 0x7FF7 THEN '北海道'
    
    ELSE '不明'
END
WHERE band_type = 0 AND terrestrial_region IS NULL;
