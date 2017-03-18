use std::ascii::AsciiExt;
use std::cmp;
use std::collections::HashMap;

use conduit::{Request, Response};
use conduit_router::RequestParams;
use diesel::associations::Identifiable;
use diesel::pg::upsert::*;
use diesel::pg::{Pg, PgConnection};
use diesel::prelude::*;
use diesel;
use diesel_full_text_search::*;
use license_exprs;
use pg::GenericConnection;
use pg::rows::Row;
use rustc_serialize::hex::ToHex;
use rustc_serialize::json;
use semver;
use time::{Timespec, Duration};
use url::Url;

use app::{App, RequestApp};
use badge::EncodableBadge;
use category::EncodableCategory;
use db::RequestTransaction;
use dependency::{Dependency, EncodableDependency};
use download::{VersionDownload, EncodableVersionDownload};
use git;
use keyword::EncodableKeyword;
use owner::{EncodableOwner, Owner, Rights, OwnerKind, Team, rights, CrateOwner};
use schema::*;
use upload;
use user::RequestUser;
use util::errors::NotFound;
use util::{read_le_u32, read_fill};
use util::{RequestUtils, CargoResult, internal, ChainError, human};
use version::EncodableVersion;
use {Model, User, Keyword, Version, Category, Badge, Replica};

#[derive(Clone, Queryable, Identifiable, AsChangeset)]
pub struct Crate {
    pub id: i32,
    pub name: String,
    pub updated_at: Timespec,
    pub created_at: Timespec,
    pub downloads: i32,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub documentation: Option<String>,
    pub readme: Option<String>,
    pub license: Option<String>,
    pub repository: Option<String>,
    pub max_upload_size: Option<i32>,
}

/// We literally never want to select textsearchable_index_col
/// so we provide this type and constant to pass to `.select`
type AllColumns = (crates::id, crates::name, crates::updated_at,
    crates::created_at, crates::downloads, crates::description,
    crates::homepage, crates::documentation, crates::readme, crates::license,
    crates::repository, crates::max_upload_size);

pub const ALL_COLUMNS: AllColumns = (crates::id, crates::name,
    crates::updated_at, crates::created_at, crates::downloads,
    crates::description, crates::homepage, crates::documentation,
    crates::readme, crates::license, crates::repository,
    crates::max_upload_size);

type CrateQuery<'a> = crates::BoxedQuery<'a, Pg, <AllColumns as Expression>::SqlType>;

#[derive(RustcEncodable, RustcDecodable)]
pub struct EncodableCrate {
    pub id: String,
    pub name: String,
    pub updated_at: String,
    pub versions: Option<Vec<i32>>,
    pub keywords: Option<Vec<String>>,
    pub categories: Option<Vec<String>>,
    pub badges: Option<Vec<EncodableBadge>>,
    pub created_at: String,
    pub downloads: i32,
    pub max_version: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub documentation: Option<String>,
    pub license: Option<String>,
    pub repository: Option<String>,
    pub links: CrateLinks,
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct CrateLinks {
    pub version_downloads: String,
    pub versions: Option<String>,
    pub owners: Option<String>,
    pub reverse_dependencies: String,
}

#[derive(Insertable, AsChangeset, Default)]
#[table_name="crates"]
#[primary_key(name, max_upload_size)] // This is actually just to skip updating them
pub struct NewCrate<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub homepage: Option<&'a str>,
    pub documentation: Option<&'a str>,
    pub readme: Option<&'a str>,
    pub repository: Option<&'a str>,
    pub license: Option<&'a str>,
    pub max_upload_size: Option<i32>,
}

impl<'a> NewCrate<'a> {
    pub fn create_or_update(
        mut self,
        conn: &PgConnection,
        license_file: Option<&str>,
        uploader: i32,
    ) -> CargoResult<Crate> {
        use diesel::update;

        self.validate(license_file)?;
        self.ensure_name_not_reserved(conn)?;

        conn.transaction(|| {
            // To avoid race conditions, we try to insert
            // first so we know whether to add an owner
            if let Some(krate) = self.save_new_crate(conn, uploader)? {
                return Ok(krate)
            }

            let target = crates::table.filter(
                canon_crate_name(crates::name)
                    .eq(canon_crate_name(self.name)));
            update(target).set(&self)
                .returning(ALL_COLUMNS)
                .get_result(conn)
                .map_err(Into::into)
        })
    }

    fn validate(&mut self, license_file: Option<&str>) -> CargoResult<()> {
        fn validate_url(url: Option<&str>, field: &str) -> CargoResult<()> {
            let url = match url {
                Some(s) => s,
                None => return Ok(())
            };
            let url = Url::parse(url).map_err(|_| {
                human(&format_args!("`{}` is not a valid url: `{}`", field, url))
            })?;
            match &url.scheme()[..] {
                "http" | "https" => {}
                s => return Err(human(&format_args!("`{}` has an invalid url \
                                                    scheme: `{}`", field, s)))
            }
            if url.cannot_be_a_base() {
                return Err(human(&format_args!("`{}` must have relative scheme \
                                               data: {}", field, url)))
            }
            Ok(())
        }

        validate_url(self.homepage, "homepage")?;
        validate_url(self.documentation, "documentation")?;
        validate_url(self.repository, "repository")?;
        self.validate_license(license_file)?;
        Ok(())
    }

    fn validate_license(&mut self, license_file: Option<&str>) -> CargoResult<()> {
        if let Some(ref license) = self.license {
            for part in license.split("/") {
               license_exprs::validate_license_expr(part)
                   .map_err(|e| human(&format_args!("{}; see http://opensource.org/licenses \
                                                    for options, and http://spdx.org/licenses/ \
                                                    for their identifiers", e)))?;
            }
        } else if license_file.is_some() {
            // If no license is given, but a license file is given, flag this
            // crate as having a nonstandard license. Note that we don't
            // actually do anything else with license_file currently.
            self.license = Some("non-standard");
        }
        Ok(())
    }

    fn ensure_name_not_reserved(&self, conn: &PgConnection) -> CargoResult<()> {
        use schema::reserved_crate_names::dsl::*;
        use diesel::select;
        use diesel::expression::dsl::exists;

        let reserved_name = select(exists(reserved_crate_names
            .filter(canon_crate_name(name).eq(canon_crate_name(self.name)))
            )).get_result::<bool>(conn)?;
        if reserved_name {
            Err(human("cannot upload a crate with a reserved name"))
        } else {
            Ok(())
        }
    }

    fn save_new_crate(&self, conn: &PgConnection, user_id: i32) -> CargoResult<Option<Crate>> {
        use schema::crates::dsl::*;
        use diesel::insert;

        conn.transaction(|| {
            let maybe_inserted = insert(&self.on_conflict_do_nothing()).into(crates)
                .returning(ALL_COLUMNS)
                .get_result::<Crate>(conn)
                .optional()?;

            if let Some(ref krate) = maybe_inserted {
                let owner = CrateOwner {
                    crate_id: krate.id,
                    owner_id: user_id,
                    created_by: user_id,
                    owner_kind: OwnerKind::User as i32,
                };
                insert(&owner).into(crate_owners::table)
                    .execute(conn)?;
            }

            Ok(maybe_inserted)
        })
    }
}

impl Crate {
    pub fn by_name(name: &str) -> CrateQuery {
        crates::table
            .select(ALL_COLUMNS)
            .filter(
                canon_crate_name(crates::name).eq(
                    canon_crate_name(name))
            ).into_boxed()
    }

    pub fn find_by_name(conn: &GenericConnection,
                        name: &str) -> CargoResult<Crate> {
        let stmt = conn.prepare("SELECT * FROM crates \
                                      WHERE canon_crate_name(name) =
                                            canon_crate_name($1) LIMIT 1")?;
        let rows = stmt.query(&[&name])?;
        let row = rows.iter().next();
        let row = row.chain_error(|| NotFound)?;
        Ok(Model::from_row(&row))
    }

    pub fn find_or_insert(conn: &GenericConnection,
                          name: &str,
                          user_id: i32,
                          description: &Option<String>,
                          homepage: &Option<String>,
                          documentation: &Option<String>,
                          readme: &Option<String>,
                          repository: &Option<String>,
                          license: &Option<String>,
                          license_file: &Option<String>,
                          max_upload_size: Option<i32>)
                          -> CargoResult<Crate> {
        let description = description.as_ref().map(|s| &s[..]);
        let homepage = homepage.as_ref().map(|s| &s[..]);
        let documentation = documentation.as_ref().map(|s| &s[..]);
        let readme = readme.as_ref().map(|s| &s[..]);
        let repository = repository.as_ref().map(|s| &s[..]);
        let mut license = license.as_ref().map(|s| &s[..]);
        let license_file = license_file.as_ref().map(|s| &s[..]);
        validate_url(homepage, "homepage")?;
        validate_url(documentation, "documentation")?;
        validate_url(repository, "repository")?;

        match license {
            // If a license is given, validate it to make sure it's actually a
            // valid license
            Some(..) => validate_license(license)?,

            // If no license is given, but a license file is given, flag this
            // crate as having a nonstandard license. Note that we don't
            // actually do anything else with license_file currently.
            None if license_file.is_some() => {
                license = Some("non-standard");
            }

            None => {}
        }

        // TODO: like with users, this is sadly racy
        let stmt = conn.prepare("UPDATE crates
                                         SET documentation = $1,
                                             homepage = $2,
                                             description = $3,
                                             readme = $4,
                                             license = $5,
                                             repository = $6
                                       WHERE canon_crate_name(name) =
                                             canon_crate_name($7)
                                   RETURNING *")?;
        let rows = stmt.query(&[&documentation, &homepage,
            &description, &readme,
            &license, &repository,
            &name])?;
        match rows.iter().next() {
            Some(row) => return Ok(Model::from_row(&row)),
            None => {}
        }

        let stmt = conn.prepare("SELECT 1 FROM reserved_crate_names
                                 WHERE canon_crate_name(name) =
                                       canon_crate_name($1)")?;
        let rows = stmt.query(&[&name])?;
        if !rows.is_empty() {
            return Err(human("cannot upload a crate with a reserved name"))
        }

        let stmt = conn.prepare("INSERT INTO crates
                                      (name, description, homepage,
                                       documentation, readme,
                                       repository, license, max_upload_size)
                                      VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                                      RETURNING *")?;
        let rows = stmt.query(&[&name, &description, &homepage,
            &documentation, &readme,
            &repository, &license, &max_upload_size])?;
        let ret: Crate = Model::from_row(&rows.iter().next().chain_error(|| {
            internal("no crate returned")
        })?);

        conn.execute("INSERT INTO crate_owners
                           (crate_id, owner_id, created_by, owner_kind)
                           VALUES ($1, $2, $2, $3)",
                     &[&ret.id, &user_id, &(OwnerKind::User as i32)])?;
        return Ok(ret);

        fn validate_url(url: Option<&str>, field: &str) -> CargoResult<()> {
            let url = match url {
                Some(s) => s,
                None => return Ok(())
            };
            let url = Url::parse(url).map_err(|_| {
                human(&format_args!("`{}` is not a valid url: `{}`", field, url))
            })?;
            match &url.scheme()[..] {
                "http" | "https" => {}
                s => return Err(human(&format_args!("`{}` has an invalid url \
                                               scheme: `{}`", field, s)))
            }
            if url.cannot_be_a_base() {
                return Err(human(&format_args!("`{}` must have relative scheme \
                                                        data: {}", field, url)))
            }
            Ok(())
        }

        fn validate_license(license: Option<&str>) -> CargoResult<()> {
            license.iter().flat_map(|s| s.split("/"))
                   .map(license_exprs::validate_license_expr)
                   .collect::<Result<Vec<_>, _>>()
                   .map(|_| ())
                   .map_err(|e| human(&format_args!("{}; see http://opensource.org/licenses \
                                                  for options, and http://spdx.org/licenses/ \
                                                  for their identifiers", e)))
        }

    }

    pub fn valid_name(name: &str) -> bool {
        if name.len() == 0 { return false }
        name.chars().next().unwrap().is_alphabetic() &&
            name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') &&
            name.chars().all(|c| c.is_ascii())
    }

    pub fn valid_feature_name(name: &str) -> bool {
        let mut parts = name.split('/');
        match parts.next() {
            Some(part) if !Crate::valid_name(part) => return false,
            None => return false,
            _ => {}
        }
        match parts.next() {
            Some(part) if !Crate::valid_name(part) => return false,
            _ => {}
        }
        parts.next().is_none()
    }

    pub fn minimal_encodable(self,
                             max_version: semver::Version,
                             badges: Option<Vec<Badge>>) -> EncodableCrate {
        self.encodable(max_version, None, None, None, badges)
    }

    pub fn encodable(self,
                     max_version: semver::Version,
                     versions: Option<Vec<i32>>,
                     keywords: Option<&[Keyword]>,
                     categories: Option<&[Category]>,
                     badges: Option<Vec<Badge>>)
                     -> EncodableCrate {
        let Crate {
            name, created_at, updated_at, downloads, description,
            homepage, documentation, license, repository,
            readme: _, id: _, max_upload_size: _,
        } = self;
        let versions_link = match versions {
            Some(..) => None,
            None => Some(format!("/api/v1/crates/{}/versions", name)),
        };
        let keyword_ids = keywords.map(|kws| kws.iter().map(|kw| kw.keyword.clone()).collect());
        let category_ids = categories.map(|cats| cats.iter().map(|cat| cat.slug.clone()).collect());
        let badges = badges.map(|bs| {
            bs.into_iter().map(|b| b.encodable()).collect()
        });
        EncodableCrate {
            id: name.clone(),
            name: name.clone(),
            updated_at: ::encode_time(updated_at),
            created_at: ::encode_time(created_at),
            downloads: downloads,
            versions: versions,
            keywords: keyword_ids,
            categories: category_ids,
            badges: badges,
            max_version: max_version.to_string(),
            documentation: documentation,
            homepage: homepage,
            description: description,
            license: license,
            repository: repository,
            links: CrateLinks {
                version_downloads: format!("/api/v1/crates/{}/downloads", name),
                versions: versions_link,
                owners: Some(format!("/api/v1/crates/{}/owners", name)),
                reverse_dependencies: format!("/api/v1/crates/{}/reverse_dependencies", name)
            },
        }
    }

    pub fn max_version(&self, conn: &GenericConnection) -> CargoResult<semver::Version> {
        let stmt = conn.prepare("SELECT num FROM versions WHERE crate_id = $1
                                 AND yanked = 'f'")?;
        let rows = stmt.query(&[&self.id])?;
        Ok(Version::max(rows.iter().map(|r| r.get::<_, String>("num"))
           .map(|s| semver::Version::parse(&s).unwrap())))
    }

    pub fn versions(&self, conn: &GenericConnection) -> CargoResult<Vec<Version>> {
        let stmt = conn.prepare("SELECT * FROM versions \
                                      WHERE crate_id = $1")?;
        let rows = stmt.query(&[&self.id])?;
        let mut ret = rows.iter().map(|r| {
            Model::from_row(&r)
        }).collect::<Vec<Version>>();
        ret.sort_by(|a, b| b.num.cmp(&a.num));
        Ok(ret)
    }

    pub fn owners(&self, conn: &GenericConnection) -> CargoResult<Vec<Owner>> {
        let stmt = conn.prepare("SELECT * FROM users
                                      INNER JOIN crate_owners
                                         ON crate_owners.owner_id = users.id
                                      WHERE crate_owners.crate_id = $1
                                        AND crate_owners.deleted = FALSE
                                        AND crate_owners.owner_kind = $2")?;
        let user_rows = stmt.query(&[&self.id, &(OwnerKind::User as i32)])?;

        let stmt = conn.prepare("SELECT * FROM teams
                                      INNER JOIN crate_owners
                                         ON crate_owners.owner_id = teams.id
                                      WHERE crate_owners.crate_id = $1
                                        AND crate_owners.deleted = FALSE
                                        AND crate_owners.owner_kind = $2")?;
        let team_rows = stmt.query(&[&self.id, &(OwnerKind::Team as i32)])?;

        let mut owners = vec![];
        owners.extend(user_rows.iter().map(|r| Owner::User(Model::from_row(&r))));
        owners.extend(team_rows.iter().map(|r| Owner::Team(Model::from_row(&r))));
        Ok(owners)
    }

    pub fn owner_add(&self, app: &App, conn: &GenericConnection, req_user: &User,
                     login: &str) -> CargoResult<()> {
        let owner = match Owner::find_by_login(conn, login) {
            Ok(owner @ Owner::User(_)) => { owner }
            Ok(Owner::Team(team)) => if team.contains_user(app, req_user)? {
                Owner::Team(team)
            } else {
                return Err(human(&format_args!("only members of {} can add it as \
                                          an owner", login)));
            },
            Err(err) => if login.contains(":") {
                Owner::Team(Team::create(app, conn, login, req_user)?)
            } else {
                return Err(err);
            },
        };

        // First try to un-delete if they've been soft deleted previously, then
        // do an insert if that didn't actually affect anything.
        let amt = conn.execute("UPDATE crate_owners
                                        SET deleted = FALSE
                                      WHERE crate_id = $1 AND owner_id = $2
                                        AND owner_kind = $3",
                               &[&self.id, &owner.id(), &owner.kind()])?;
        assert!(amt <= 1);
        if amt == 0 {
            conn.execute("INSERT INTO crate_owners
                               (crate_id, owner_id, created_by, owner_kind)
                               VALUES ($1, $2, $3, $4)",
                         &[&self.id, &owner.id(), &req_user.id,
                             &owner.kind()])?;
        }

        Ok(())
    }

    pub fn owner_remove(&self,
                        conn: &GenericConnection,
                        _req_user: &User,
                        login: &str) -> CargoResult<()> {
        let owner = Owner::find_by_login(conn, login).map_err(|_| {
            human(&format_args!("could not find owner with login `{}`", login))
        })?;
        conn.execute("UPDATE crate_owners
                              SET deleted = TRUE
                            WHERE crate_id = $1 AND owner_id = $2
                              AND owner_kind = $3",
                     &[&self.id, &owner.id(), &owner.kind()])?;
        Ok(())
    }

    pub fn add_version(&mut self,
                       conn: &GenericConnection,
                       ver: &semver::Version,
                       features: &HashMap<String, Vec<String>>,
                       authors: &[String])
                       -> CargoResult<Version> {
        match Version::find_by_num(conn, self.id, ver)? {
            Some(..) => {
                return Err(human(&format_args!("crate version `{}` is already uploaded",
                                         ver)))
            }
            None => {}
        }
        Version::insert(conn, self.id, ver, features, authors)
    }

    pub fn keywords(&self, conn: &GenericConnection) -> CargoResult<Vec<Keyword>> {
        let stmt = conn.prepare("SELECT keywords.* FROM keywords
                                      LEFT JOIN crates_keywords
                                      ON keywords.id = crates_keywords.keyword_id
                                      WHERE crates_keywords.crate_id = $1")?;
        let rows = stmt.query(&[&self.id])?;
        Ok(rows.iter().map(|r| Model::from_row(&r)).collect())
    }

    pub fn categories(&self, conn: &GenericConnection) -> CargoResult<Vec<Category>> {
        let stmt = conn.prepare("SELECT categories.* FROM categories \
                                      LEFT JOIN crates_categories \
                                      ON categories.id = \
                                         crates_categories.category_id \
                                      WHERE crates_categories.crate_id = $1")?;
        let rows = stmt.query(&[&self.id])?;
        Ok(rows.iter().map(|r| Model::from_row(&r)).collect())
    }

    pub fn badges(&self, conn: &GenericConnection) -> CargoResult<Vec<Badge>> {
        let stmt = conn.prepare("SELECT badges.* from badges \
                                      WHERE badges.crate_id = $1")?;
        let rows = stmt.query(&[&self.id])?;
        Ok(rows.iter().map(|r| Model::from_row(&r)).collect())
    }

    /// Returns (dependency, dependent crate name, dependent crate downloads)
    pub fn reverse_dependencies(&self,
                                conn: &GenericConnection,
                                offset: i64,
                                limit: i64)
                                -> CargoResult<(Vec<(Dependency, String, i32)>, i64)> {
        let stmt = conn.prepare(include_str!("krate_reverse_dependencies.sql"))?;

        let rows = stmt.query(&[&self.id, &offset, &limit])?;
        let cnt = if rows.is_empty() {
            0i64
        } else {
            rows.get(0).get("total")
        };
        let vec: Vec<_> = rows
            .iter()
            .map(|r| (Model::from_row(&r), r.get("crate_name"), r.get("crate_downloads")))
            .collect();

        Ok((vec, cnt))
    }
}

impl Model for Crate {
    fn from_row(row: &Row) -> Crate {
        Crate {
            id: row.get("id"),
            name: row.get("name"),
            updated_at: row.get("updated_at"),
            created_at: row.get("created_at"),
            downloads: row.get("downloads"),
            description: row.get("description"),
            documentation: row.get("documentation"),
            homepage: row.get("homepage"),
            readme: row.get("readme"),
            license: row.get("license"),
            repository: row.get("repository"),
            max_upload_size: row.get("max_upload_size"),
        }
    }
    fn table_name(_: Option<Crate>) -> &'static str { "crates" }
}

/// Handles the `GET /crates` route.
#[allow(trivial_casts)]
pub fn index(req: &mut Request) -> CargoResult<Response> {
    use diesel::expression::dsl::sql;
    use diesel::types::BigInt;

    let conn = req.db_conn()?;
    let (offset, limit) = req.pagination(10, 100)?;
    let params = req.query();
    let sort = params.get("sort").map(|s| &**s).unwrap_or("alpha");

    let mut query = crates::table
        .select((ALL_COLUMNS, sql::<BigInt>("COUNT(*) OVER ()")))
        .limit(limit)
        .offset(offset)
        .into_boxed();

    if sort == "downloads" {
        query = query.order(crates::downloads.desc())
    } else {
        query = query.order(crates::name.asc())
    }

    if let Some(q_string) = params.get("q") {
        let q = plainto_tsquery(q_string);
        query = query.filter(q.matches(crates::textsearchable_index_col));

        let perfect_match = crates::name.eq(q_string).desc();
        if sort == "downloads" {
            query = query.order((perfect_match, crates::downloads.desc()));
        } else {
            let rank = ts_rank_cd(crates::textsearchable_index_col, q);
            query = query.order((perfect_match, rank.desc()))
        }
    } else if let Some(letter) = params.get("letter") {
        let pattern = format!("{}%", letter.chars().next().unwrap()
                                       .to_lowercase().collect::<String>());
        query = query.filter(canon_crate_name(crates::name).like(pattern));
    } else if let Some(kw) = params.get("keyword") {
        query = query.filter(crates::id.eq_any(
            crates_keywords::table.select(crates_keywords::crate_id)
                .inner_join(keywords::table)
                .filter(lower(keywords::keyword).eq(lower(kw)))
        ));
    } else if let Some(cat) = params.get("category") {
        query = query.filter(crates::id.eq_any(
            crates_categories::table.select(crates_categories::crate_id)
                .inner_join(categories::table)
                .filter(categories::slug.eq(cat).or(
                        categories::slug.like(format!("{}::%", cat))))
        ));
    } else if let Some(user_id) = params.get("user_id").and_then(|s| s.parse::<i32>().ok()) {
        query = query.filter(crates::id.eq_any((
            crate_owners::table.select(crate_owners::crate_id)
                .filter(crate_owners::owner_id.eq(user_id))
                .filter(crate_owners::owner_kind.eq(OwnerKind::User as i32))
        )));
    } else if params.get("following").is_some() {
        query = query.filter(crates::id.eq_any((
            follows::table.select(follows::crate_id)
                .filter(follows::user_id.eq(req.user()?.id))
        )));
    }

    let data = query.load::<(Crate, i64)>(&*conn)?;
    let total = data.get(0).map(|&(_, t)| t).unwrap_or(0);
    let crates = data.into_iter().map(|(c, _)| c).collect::<Vec<_>>();

    let versions = Version::belonging_to(&crates)
        .load::<Version>(&*conn)?
        .grouped_by(&crates)
        .into_iter()
        .map(|versions| Version::max(versions.into_iter().map(|v| v.num)));

    let crates = versions.zip(crates).map(|(max_version, krate)| {
        // FIXME: If we add crate_id to the Badge enum we can eliminate
        // this N+1
        let badges = badges::table.filter(badges::crate_id.eq(krate.id))
            .load::<Badge>(&*conn)?;
        Ok(krate.minimal_encodable(max_version, Some(badges)))
    }).collect::<Result<_, ::diesel::result::Error>>()?;

    #[derive(RustcEncodable)]
    struct R { crates: Vec<EncodableCrate>, meta: Meta }
    #[derive(RustcEncodable)]
    struct Meta { total: i64 }

    Ok(req.json(&R {
        crates: crates,
        meta: Meta { total: total },
    }))
}

/// Handles the `GET /summary` route.
pub fn summary(req: &mut Request) -> CargoResult<Response> {
    use schema::crates::dsl::*;

    let conn = req.db_conn()?;
    let num_crates = crates.count().get_result(&*conn)?;
    let num_downloads = metadata::table.select(metadata::total_downloads)
        .get_result(&*conn)?;

    let encode_crates = |krates: Vec<Crate>| -> CargoResult<Vec<_>> {
        Version::belonging_to(&krates)
            .filter(versions::yanked.eq(false))
            .load::<Version>(&*conn)?
            .grouped_by(&krates)
            .into_iter()
            .map(|versions| Version::max(versions.into_iter().map(|v| v.num)))
            .zip(krates)
            .map(|(max_version, krate)| {
                 Ok(krate.minimal_encodable(max_version, None))
            }).collect()
    };

    let new_crates = crates.order(created_at.desc())
        .select(ALL_COLUMNS)
        .limit(10)
        .load(&*conn)?;
    let just_updated = crates.filter(updated_at.ne(created_at))
        .order(updated_at.desc())
        .select(ALL_COLUMNS)
        .limit(10)
        .load(&*conn)?;
    let most_downloaded = crates.order(downloads.desc())
        .select(ALL_COLUMNS)
        .limit(10)
        .load(&*conn)?;

    let popular_keywords = keywords::table.order(keywords::crates_cnt.desc())
        .limit(10)
        .load(&*conn)?
        .into_iter()
        .map(Keyword::encodable)
        .collect();

    let popular_categories = Category::toplevel(&conn, "crates", 10, 0)?
        .into_iter()
        .map(Category::encodable)
        .collect();

    #[derive(RustcEncodable)]
    struct R {
        num_downloads: i64,
        num_crates: i64,
        new_crates: Vec<EncodableCrate>,
        most_downloaded: Vec<EncodableCrate>,
        just_updated: Vec<EncodableCrate>,
        popular_keywords: Vec<EncodableKeyword>,
        popular_categories: Vec<EncodableCategory>,
    }
    Ok(req.json(&R {
        num_downloads: num_downloads,
        num_crates: num_crates,
        new_crates: encode_crates(new_crates)?,
        most_downloaded: encode_crates(most_downloaded)?,
        just_updated: encode_crates(just_updated)?,
        popular_keywords: popular_keywords,
        popular_categories: popular_categories,
    }))
}

/// Handles the `GET /crates/:crate_id` route.
pub fn show(req: &mut Request) -> CargoResult<Response> {
    let name = &req.params()["crate_id"];
    let conn = req.tx()?;
    let krate = Crate::find_by_name(conn, &name)?;
    let versions = krate.versions(conn)?;
    let ids = versions.iter().map(|v| v.id).collect();
    let kws = krate.keywords(conn)?;
    let cats = krate.categories(conn)?;
    let badges = krate.badges(conn)?;
    let max_version = krate.max_version(conn)?;

    #[derive(RustcEncodable)]
    struct R {
        krate: EncodableCrate,
        versions: Vec<EncodableVersion>,
        keywords: Vec<EncodableKeyword>,
        categories: Vec<EncodableCategory>,
    }
    Ok(req.json(&R {
        krate: krate.clone().encodable(
            max_version, Some(ids), Some(&kws), Some(&cats), Some(badges)
        ),
        versions: versions.into_iter().map(|v| {
            v.encodable(&krate.name)
        }).collect(),
        keywords: kws.into_iter().map(|k| k.encodable()).collect(),
        categories: cats.into_iter().map(|k| k.encodable()).collect(),
    }))
}

/// Handles the `PUT /crates/new` route.
pub fn new(req: &mut Request) -> CargoResult<Response> {
    let app = req.app().clone();

    let (new_crate, user) = parse_new_headers(req)?;
    let name = &*new_crate.name;
    let vers = &*new_crate.vers;
    let features = new_crate.features.iter().map(|(k, v)| {
        (k[..].to_string(), v.iter().map(|v| v[..].to_string()).collect())
    }).collect::<HashMap<String, Vec<String>>>();
    let keywords = new_crate.keywords.as_ref().map(|s| &s[..])
                                     .unwrap_or(&[]);
    let keywords = keywords.iter().map(|k| k[..].to_string()).collect::<Vec<_>>();

    let categories = new_crate.categories.as_ref().map(|s| &s[..])
                                     .unwrap_or(&[]);
    let categories: Vec<_> = categories.iter().map(|k| k[..].to_string()).collect();

    // Persist the new crate, if it doesn't already exist
    let mut krate = Crate::find_or_insert(req.tx()?, name, user.id,
                                          &new_crate.description,
                                          &new_crate.homepage,
                                          &new_crate.documentation,
                                          &new_crate.readme,
                                          &new_crate.repository,
                                          &new_crate.license,
                                          &new_crate.license_file,
                                          None)?;

    let owners = krate.owners(req.tx()?)?;
    if rights(req.app(), &owners, &user)? < Rights::Publish {
        return Err(human("crate name has already been claimed by \
                          another user"))
    }

    if krate.name != name {
        return Err(human(&format_args!("crate was previously named `{}`", krate.name)))
    }

    let length = req.content_length().chain_error(|| {
        human("missing header: Content-Length")
    })?;
    let max = krate.max_upload_size.map(|m| m as u64)
                   .unwrap_or(app.config.max_upload_size);
    if length > max {
        return Err(human(&format_args!("max upload size is: {}", max)))
    }

    // Persist the new version of this crate
    let mut version = krate.add_version(req.tx()?, vers, &features,
                                        &new_crate.authors)?;

    // Link this new version to all dependencies
    let mut deps = Vec::new();
    for dep in new_crate.deps.iter() {
        let (dep, krate) = version.add_dependency(req.tx()?, dep)?;
        deps.push(dep.git_encode(&krate.name));
    }

    // Update all keywords for this crate
    Keyword::update_crate_old(req.tx()?, &krate, &keywords)?;

    // Update all categories for this crate, collecting any invalid categories
    // in order to be able to warn about them
    let ignored_invalid_categories = Category::update_crate_old(req.tx()?, &krate, &categories)?;

    // Update all badges for this crate, collecting any invalid badges in
    // order to be able to warn about them
    let ignored_invalid_badges = Badge::update_crate(
        req.tx()?,
        &krate,
        new_crate.badges.unwrap_or_else(HashMap::new)
    )?;
    let max_version = krate.max_version(req.tx()?)?;

    // Upload the crate, return way to delete the crate from the server
    // If the git commands fail below, we shouldn't keep the crate on the
    // server.
    let (cksum, mut bomb) = app.config.uploader.upload(req, &krate, max, &vers)?;

    // Register this crate in our local git repo.
    let git_crate = git::Crate {
        name: name.to_string(),
        vers: vers.to_string(),
        cksum: cksum.to_hex(),
        features: features,
        deps: deps,
        yanked: Some(false),
    };
    git::add_crate(&**req.app(), &git_crate).chain_error(|| {
        internal(&format_args!("could not add crate `{}` to the git repo", name))
    })?;

    // Now that we've come this far, we're committed!
    bomb.path = None;

    #[derive(RustcEncodable)]
    struct Warnings {
        invalid_categories: Vec<String>,
        invalid_badges: Vec<String>,
    }
    let warnings = Warnings {
        invalid_categories: ignored_invalid_categories,
        invalid_badges: ignored_invalid_badges,
    };

    #[derive(RustcEncodable)]
    struct R { krate: EncodableCrate, warnings: Warnings }
    Ok(req.json(&R {
        krate: krate.minimal_encodable(max_version, None),
        warnings: warnings
    }))
}

fn parse_new_headers(req: &mut Request) -> CargoResult<(upload::NewCrate, User)> {
    // Read the json upload request
    let amt = read_le_u32(req.body())? as u64;
    let max = req.app().config.max_upload_size;
    if amt > max {
        return Err(human(&format_args!("max upload size is: {}", max)))
    }
    let mut json = vec![0; amt as usize];
    read_fill(req.body(), &mut json)?;
    let json = String::from_utf8(json).map_err(|_| {
        human("json body was not valid utf-8")
    })?;
    let new: upload::NewCrate = json::decode(&json).map_err(|e| {
        human(&format_args!("invalid upload request: {:?}", e))
    })?;

    // Make sure required fields are provided
    fn empty(s: Option<&String>) -> bool { s.map_or(true, |s| s.is_empty()) }
    let mut missing = Vec::new();

    if empty(new.description.as_ref()) {
        missing.push("description");
    }
    if empty(new.license.as_ref()) && empty(new.license_file.as_ref()) {
        missing.push("license");
    }
    if new.authors.len() == 0 || new.authors.iter().all(|s| s.is_empty()) {
        missing.push("authors");
    }
    if missing.len() > 0 {
        return Err(human(&format_args!("missing or empty metadata fields: {}. Please \
            see http://doc.crates.io/manifest.html#package-metadata for \
            how to upload metadata", missing.join(", "))));
    }

    let user = req.user()?;
    Ok((new, user.clone()))
}

/// Handles the `GET /crates/:crate_id/:version/download` route.
pub fn download(req: &mut Request) -> CargoResult<Response> {
    let crate_name = &req.params()["crate_id"];
    let version = &req.params()["version"];

    // If we are a mirror, ignore failure to update download counts.
    // API-only mirrors won't have any crates in their database, and
    // incrementing the download count will look up the crate in the
    // database. Mirrors just want to pass along a redirect URL.
    if req.app().config.mirror == Replica::ReadOnlyMirror {
        let _ = increment_download_counts(req, crate_name, version);
    } else {
        increment_download_counts(req, crate_name, version)?;
    }

    let redirect_url = req.app().config.uploader
        .crate_location(crate_name, version).ok_or_else(||
            human("crate files not found")
        )?;

    if req.wants_json() {
        #[derive(RustcEncodable)]
        struct R { url: String }
        Ok(req.json(&R{ url: redirect_url }))
    } else {
        Ok(req.redirect(redirect_url))
    }
}

fn increment_download_counts(req: &Request, crate_name: &str, version: &str) -> CargoResult<()> {
    let tx = req.tx()?;
    let stmt = tx.prepare("SELECT versions.id as version_id
                                FROM crates
                                INNER JOIN versions ON
                                    crates.id = versions.crate_id
                                WHERE canon_crate_name(crates.name) =
                                      canon_crate_name($1)
                                  AND versions.num = $2
                                LIMIT 1")?;
    let rows = stmt.query(&[&crate_name, &version])?;
    let row = rows.iter().next().chain_error(|| {
        human("crate or version not found")
    })?;
    let version_id: i32 = row.get("version_id");
    let now = ::now();

    // Bump download counts.
    //
    // Note that this is *not* an atomic update, and that's somewhat
    // intentional. It doesn't appear that postgres supports an atomic update of
    // a counter, so we just do the hopefully "least racy" thing. This is
    // largely ok because these download counters are just that, counters. No
    // need to have super high-fidelity counter.
    //
    // Also, we only update the counter for *today*, nothing else. We have lots
    // of other counters, but they're all updated later on via the
    // update-downloads script.
    let amt = tx.execute("UPDATE version_downloads
                               SET downloads = downloads + 1
                               WHERE version_id = $1 AND date($2) = date(date)",
                         &[&version_id, &now])?;
    if amt == 0 {
        tx.execute("INSERT INTO version_downloads
                         (version_id) VALUES ($1)", &[&version_id])?;
    }
    Ok(())
}

/// Handles the `GET /crates/:crate_id/downloads` route.
pub fn downloads(req: &mut Request) -> CargoResult<Response> {
    let crate_name = &req.params()["crate_id"];
    let tx = req.tx()?;
    let krate = Crate::find_by_name(tx, crate_name)?;
    let mut versions = krate.versions(tx)?;
    versions.sort_by(|a, b| b.num.cmp(&a.num));


    let to_show = &versions[..cmp::min(5, versions.len())];
    let ids = to_show.iter().map(|i| i.id).collect::<Vec<_>>();

    let cutoff_date = ::now() + Duration::days(-90);
    let stmt = tx.prepare("SELECT * FROM version_downloads
                                 WHERE date > $1
                                   AND version_id = ANY($2)
                                 ORDER BY date ASC")?;
    let downloads = stmt.query(&[&cutoff_date, &ids])?.iter().map(|row| {
        VersionDownload::from_row(&row).encodable()
    }).collect::<Vec<_>>();

    let stmt = tx.prepare("\
          SELECT COALESCE(to_char(DATE(version_downloads.date), 'YYYY-MM-DD'), '') AS date,
                 SUM(version_downloads.downloads) AS downloads
            FROM version_downloads
           INNER JOIN versions ON
                 version_id = versions.id
           WHERE version_downloads.date > $1
             AND versions.crate_id = $2
             AND versions.id != ALL($3)
        GROUP BY DATE(version_downloads.date)
        ORDER BY DATE(version_downloads.date) ASC")?;
    let extra = stmt.query(&[&cutoff_date, &krate.id, &ids])?.iter().map(|row| {
        ExtraDownload {
            downloads: row.get("downloads"),
            date: row.get("date")
        }
    }).collect::<Vec<_>>();

    #[derive(RustcEncodable)]
    struct ExtraDownload { date: String, downloads: i64 }
    #[derive(RustcEncodable)]
    struct R { version_downloads: Vec<EncodableVersionDownload>, meta: Meta }
    #[derive(RustcEncodable)]
    struct Meta { extra_downloads: Vec<ExtraDownload> }
    let meta = Meta { extra_downloads: extra };
    Ok(req.json(&R{ version_downloads: downloads, meta: meta }))
}

fn user_and_crate(req: &mut Request) -> CargoResult<(User, Crate)> {
    let user = req.user()?;
    let crate_name = &req.params()["crate_id"];
    let tx = req.tx()?;
    let krate = Crate::find_by_name(tx, crate_name)?;
    Ok((user.clone(), krate))
}

#[derive(Insertable, Queryable, Identifiable, Associations)]
#[belongs_to(User)]
#[primary_key(user_id, crate_id)]
#[table_name="follows"]
pub struct Follow {
    user_id: i32,
    crate_id: i32,
}

fn follow_target(req: &mut Request) -> CargoResult<Follow> {
    let user = req.user()?;
    let conn = req.db_conn()?;
    let crate_name = &req.params()["crate_id"];
    let crate_id = Crate::by_name(crate_name)
        .select(crates::id)
        .first(&*conn)?;
    Ok(Follow {
        user_id: user.id,
        crate_id: crate_id,
    })
}

/// Handles the `PUT /crates/:crate_id/follow` route.
pub fn follow(req: &mut Request) -> CargoResult<Response> {
    let follow = follow_target(req)?;
    let conn = req.db_conn()?;
    diesel::insert(&follow.on_conflict_do_nothing())
        .into(follows::table)
        .execute(&*conn)?;
    #[derive(RustcEncodable)]
    struct R { ok: bool }
    Ok(req.json(&R { ok: true }))
}

/// Handles the `DELETE /crates/:crate_id/follow` route.
pub fn unfollow(req: &mut Request) -> CargoResult<Response> {
    let follow = follow_target(req)?;
    let conn = req.db_conn()?;
    diesel::delete(&follow).execute(&*conn)?;
    #[derive(RustcEncodable)]
    struct R { ok: bool }
    Ok(req.json(&R { ok: true }))
}

/// Handles the `GET /crates/:crate_id/following` route.
pub fn following(req: &mut Request) -> CargoResult<Response> {
    use diesel::expression::dsl::exists;

    let follow = follow_target(req)?;
    let conn = req.db_conn()?;
    let following = diesel::select(exists(follows::table.find(follow.id())))
        .get_result(&*conn)?;
    #[derive(RustcEncodable)]
    struct R { following: bool }
    Ok(req.json(&R { following: following }))
}

/// Handles the `GET /crates/:crate_id/versions` route.
pub fn versions(req: &mut Request) -> CargoResult<Response> {
    let crate_name = &req.params()["crate_id"];
    let tx = req.tx()?;
    let krate = Crate::find_by_name(tx, crate_name)?;
    let versions = krate.versions(tx)?;
    let versions = versions.into_iter().map(|v| v.encodable(crate_name))
                           .collect();

    #[derive(RustcEncodable)]
    struct R { versions: Vec<EncodableVersion> }
    Ok(req.json(&R{ versions: versions }))
}

/// Handles the `GET /crates/:crate_id/owners` route.
pub fn owners(req: &mut Request) -> CargoResult<Response> {
    let crate_name = &req.params()["crate_id"];
    let tx = req.tx()?;
    let krate = Crate::find_by_name(tx, crate_name)?;
    let owners = krate.owners(tx)?;
    let owners = owners.into_iter().map(|o| o.encodable()).collect();

    #[derive(RustcEncodable)]
    struct R { users: Vec<EncodableOwner> }
    Ok(req.json(&R{ users: owners }))
}

/// Handles the `PUT /crates/:crate_id/owners` route.
pub fn add_owners(req: &mut Request) -> CargoResult<Response> {
    modify_owners(req, true)
}

/// Handles the `DELETE /crates/:crate_id/owners` route.
pub fn remove_owners(req: &mut Request) -> CargoResult<Response> {
    modify_owners(req, false)
}

fn modify_owners(req: &mut Request, add: bool) -> CargoResult<Response> {
    let mut body = String::new();
    req.body().read_to_string(&mut body)?;
    let (user, krate) = user_and_crate(req)?;
    let tx = req.tx()?;
    let owners = krate.owners(tx)?;

    match rights(req.app(), &owners, &user)? {
        Rights::Full => {} // Yes!
        Rights::Publish => {
            return Err(human("team members don't have permission to modify owners"));
        }
        Rights::None => {
            return Err(human("only owners have permission to modify owners"));
        }
    }

    #[derive(RustcDecodable)]
    struct Request {
        // identical, for back-compat (owners preferred)
        users: Option<Vec<String>>,
        owners: Option<Vec<String>>,
    }

    let request: Request = json::decode(&body).map_err(|_| {
        human("invalid json request")
    })?;

    let logins = request.owners.or(request.users).ok_or_else(|| {
        human("invalid json request")
    })?;

    for login in &logins {
        if add {
            if owners.iter().any(|owner| owner.login() == *login) {
                return Err(human(&format_args!("`{}` is already an owner", login)))
            }
            krate.owner_add(req.app(), tx, &user, &login)?;
        } else {
            // Removing the team that gives you rights is prevented because
            // team members only have Rights::Publish
            if *login == user.gh_login {
                return Err(human("cannot remove yourself as an owner"))
            }
            krate.owner_remove(tx, &user, &login)?;
        }
    }

    #[derive(RustcEncodable)]
    struct R { ok: bool }
    Ok(req.json(&R{ ok: true }))
}

/// Handles the `GET /crates/:crate_id/reverse_dependencies` route.
pub fn reverse_dependencies(req: &mut Request) -> CargoResult<Response> {
    let name = &req.params()["crate_id"];
    let conn = req.tx()?;
    let krate = Crate::find_by_name(conn, &name)?;
    let (offset, limit) = req.pagination(10, 100)?;
    let (rev_deps, total) = krate.reverse_dependencies(conn, offset, limit)?;
    let rev_deps = rev_deps.into_iter()
        .map(|(dep, crate_name, downloads)| dep.encodable(&crate_name, Some(downloads)))
        .collect();

    #[derive(RustcEncodable)]
    struct R { dependencies: Vec<EncodableDependency>, meta: Meta }
    #[derive(RustcEncodable)]
    struct Meta { total: i64 }
    Ok(req.json(&R{ dependencies: rev_deps, meta: Meta { total: total } }))
}

use diesel::types::Text;
sql_function!(canon_crate_name, canon_crate_name_t, (x: Text) -> Text);
sql_function!(lower, lower_t, (x: Text) -> Text);
