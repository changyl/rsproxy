-- 初始化测试表和数据
CREATE TABLE IF NOT EXISTS users (
    id INT PRIMARY KEY AUTO_INCREMENT,
    name VARCHAR(100) NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS orders (
    id INT PRIMARY KEY AUTO_INCREMENT,
    user_id INT NOT NULL,
    amount DECIMAL(10,2) NOT NULL,
    status VARCHAR(20) DEFAULT 'pending'
);

INSERT INTO users (id, name) VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Charlie')
ON DUPLICATE KEY UPDATE name = VALUES(name);

INSERT INTO orders (id, user_id, amount, status) VALUES
    (1, 1, 99.99, 'completed'),
    (2, 1, 49.50, 'pending'),
    (3, 2, 199.00, 'completed')
ON DUPLICATE KEY UPDATE amount = VALUES(amount);
