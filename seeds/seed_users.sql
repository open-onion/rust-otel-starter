-- 种子数据：服务启动时 users 表为空才会插入，可重复跑（INSERT OR IGNORE）
INSERT OR IGNORE INTO users (id, name, email) VALUES
    ('018e0000-0000-7000-8000-000000000001', 'Ada Lovelace',  'ada@example.com'),
    ('018e0000-0000-7000-8000-000000000002', 'Alan Turing',   'alan@example.com'),
    ('018e0000-0000-7000-8000-000000000003', 'Grace Hopper',  'grace@example.com');
