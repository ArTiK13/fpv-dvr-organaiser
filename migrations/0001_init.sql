CREATE TABLE users (
 id BIGSERIAL PRIMARY KEY, telegram_id BIGINT NOT NULL UNIQUE CHECK (telegram_id > 0),
 full_name TEXT, nickname TEXT, telegram_username TEXT,
 is_admin BOOLEAN NOT NULL DEFAULT FALSE, is_public BOOLEAN NOT NULL DEFAULT TRUE,
 active BOOLEAN NOT NULL DEFAULT TRUE, notification_time TIME NOT NULL DEFAULT '08:00',
 notifications_enabled BOOLEAN NOT NULL DEFAULT TRUE,
 system_alert_interval_seconds INTEGER NOT NULL DEFAULT 3600 CHECK(system_alert_interval_seconds IN (0,900,3600,86400)),
 last_digest_date DATE, last_system_alert_at TIMESTAMPTZ,
 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE user_file_aliases (user_id BIGINT REFERENCES users NOT NULL, alias TEXT NOT NULL, PRIMARY KEY(user_id,alias));
CREATE TABLE devices (id BIGSERIAL PRIMARY KEY,name TEXT NOT NULL UNIQUE,type TEXT NOT NULL CHECK(type IN ('plane','quad','other')),active BOOLEAN NOT NULL DEFAULT TRUE);
CREATE TABLE device_aliases (device_id BIGINT REFERENCES devices NOT NULL,alias TEXT NOT NULL,PRIMARY KEY(device_id,alias));
CREATE TABLE video_systems (id BIGSERIAL PRIMARY KEY,name TEXT NOT NULL UNIQUE,with_osd BOOLEAN NOT NULL,active BOOLEAN NOT NULL DEFAULT TRUE);
CREATE TABLE video_system_aliases (video_system_id BIGINT REFERENCES video_systems NOT NULL,alias TEXT NOT NULL,PRIMARY KEY(video_system_id,alias));
CREATE TABLE settings (key TEXT PRIMARY KEY,value JSONB NOT NULL);
CREATE TABLE events (id BIGSERIAL PRIMARY KEY,name TEXT NOT NULL UNIQUE,date_start DATE,date_end DATE,date_precision TEXT,manual_date BOOLEAN NOT NULL DEFAULT FALSE);
CREATE TABLE videos (
 id BIGSERIAL PRIMARY KEY,relative_path TEXT NOT NULL UNIQUE,size_bytes BIGINT NOT NULL,mtime_ns BIGINT NOT NULL,
 classification_revision TEXT,sha256 TEXT,event_id BIGINT REFERENCES events,device_id BIGINT REFERENCES devices,video_system_id BIGINT REFERENCES video_systems,pilot_id BIGINT REFERENCES users,
 colored BOOLEAN NOT NULL DEFAULT FALSE,duration_ms BIGINT,width INTEGER,height INTEGER,fps TEXT,
 state TEXT NOT NULL DEFAULT 'discovered' CHECK(state IN ('discovered','metadata_pending','classified','unclassified','waiting_for_immich','ready','missing','conflict')),
 first_classified_at TIMESTAMPTZ,last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 immich_asset_id UUID,immich_original_path TEXT,immich_status TEXT NOT NULL DEFAULT 'pending',immich_last_seen_at TIMESTAMPTZ
);
CREATE INDEX videos_filters ON videos(pilot_id,device_id,video_system_id,state);
CREATE TABLE classification_issues (video_id BIGINT REFERENCES videos NOT NULL,issue_code TEXT NOT NULL,first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),resolved_at TIMESTAMPTZ,PRIMARY KEY(video_id,issue_code));
CREATE TABLE media_errors (relative_path TEXT PRIMARY KEY,error TEXT NOT NULL,first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),resolved_at TIMESTAMPTZ);
CREATE TABLE file_observations (relative_path TEXT PRIMARY KEY,size_bytes BIGINT NOT NULL,mtime_ns BIGINT NOT NULL,stable_since TIMESTAMPTZ NOT NULL DEFAULT now());
CREATE TABLE notification_subscriptions (user_id BIGINT REFERENCES users NOT NULL,pilot_id BIGINT REFERENCES users NOT NULL,created_at TIMESTAMPTZ NOT NULL DEFAULT now(),PRIMARY KEY(user_id,pilot_id));
CREATE TABLE notification_deliveries (user_id BIGINT REFERENCES users NOT NULL,video_id BIGINT REFERENCES videos NOT NULL,delivered_at TIMESTAMPTZ NOT NULL DEFAULT now(),PRIMARY KEY(user_id,video_id));
CREATE TABLE access_requests (telegram_id BIGINT PRIMARY KEY,username TEXT,display_name TEXT,requested_at TIMESTAMPTZ NOT NULL DEFAULT now(),resolved_at TIMESTAMPTZ);
CREATE TABLE delivery_jobs (
 id UUID PRIMARY KEY,user_id BIGINT REFERENCES users NOT NULL,chat_id BIGINT NOT NULL,mode TEXT NOT NULL CHECK(mode IN ('video','document','immich')),
 state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','running','waiting_for_immich','done','cancelled','failed')),
 total_count INTEGER NOT NULL,completed_count INTEGER NOT NULL DEFAULT 0,next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),wait_started_at TIMESTAMPTZ,error TEXT,attempts INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE delivery_job_videos(job_id UUID REFERENCES delivery_jobs NOT NULL,video_id BIGINT REFERENCES videos NOT NULL,delivered BOOLEAN NOT NULL DEFAULT FALSE,PRIMARY KEY(job_id,video_id));
CREATE TABLE immich_shared_links (
 id BIGSERIAL PRIMARY KEY,immich_id UUID UNIQUE NOT NULL,immich_key TEXT NOT NULL,user_id BIGINT REFERENCES users NOT NULL,job_id UUID REFERENCES delivery_jobs,
 expires_at TIMESTAMPTZ NOT NULL,revoke_requested BOOLEAN NOT NULL DEFAULT FALSE,revoked_at TIMESTAMPTZ,sent BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE TABLE immich_shared_link_videos(link_id BIGINT REFERENCES immich_shared_links NOT NULL,video_id BIGINT REFERENCES videos NOT NULL,PRIMARY KEY(link_id,video_id));
CREATE TABLE fallback_download_tokens (token_hash TEXT PRIMARY KEY,video_id BIGINT REFERENCES videos NOT NULL,user_id BIGINT REFERENCES users NOT NULL,expires_at TIMESTAMPTZ NOT NULL,revoked_at TIMESTAMPTZ);
CREATE TABLE system_alerts (key TEXT PRIMARY KEY,message TEXT NOT NULL,active BOOLEAN NOT NULL DEFAULT TRUE,created_at TIMESTAMPTZ NOT NULL DEFAULT now());
CREATE TABLE system_alert_deliveries (user_id BIGINT REFERENCES users NOT NULL,alert_key TEXT REFERENCES system_alerts NOT NULL,PRIMARY KEY(user_id,alert_key));
CREATE TABLE audit_log(id BIGSERIAL PRIMARY KEY,actor_id BIGINT REFERENCES users,action TEXT NOT NULL,entity TEXT NOT NULL,created_at TIMESTAMPTZ NOT NULL DEFAULT now());
