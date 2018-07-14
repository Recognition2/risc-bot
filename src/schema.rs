table! {
    chat (telegram_id) {
        telegram_id -> Bigint,
        title -> Text,
        created_at -> Datetime,
        updated_at -> Datetime,
    }
}

table! {
    chat_user_stats (id) {
        id -> Integer,
        chat_id -> Bigint,
        user_id -> Bigint,
        messages -> Integer,
        created_at -> Datetime,
        updated_at -> Datetime,
    }
}

table! {
    user (telegram_id) {
        telegram_id -> Bigint,
        name -> Nullable<Text>,
        created_at -> Datetime,
        updated_at -> Datetime,
    }
}

joinable!(chat_user_stats -> chat (chat_id));
joinable!(chat_user_stats -> user (user_id));

allow_tables_to_appear_in_same_query!(
    chat,
    chat_user_stats,
    user,
);
