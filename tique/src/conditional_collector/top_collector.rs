use std::marker::PhantomData;

use tantivy::{
    collector::{Collector, SegmentCollector},
    DocAddress, DocId, Result, Score, SegmentLocalId, SegmentReader,
};

use super::{
    topk::{TopK, TopKProvider},
    CheckCondition, ConditionForSegment,
};

pub struct TopCollector<T, P, CF> {
    limit: usize,
    condition_factory: CF,
    _score: PhantomData<T>,
    _provider: PhantomData<P>,
}

impl<T, P, CF> TopCollector<T, P, CF>
where
    T: PartialOrd,
    P: 'static + Send + Sync + TopKProvider<Score>,
    CF: ConditionForSegment<T> + Sync,
{
    pub fn new(limit: usize, condition_factory: CF) -> Self {
        if limit < 1 {
            panic!("Limit must be greater than 0");
        }
        TopCollector {
            limit,
            condition_factory,
            _score: PhantomData,
            _provider: PhantomData,
        }
    }
}

impl<P, CF> Collector for TopCollector<Score, P, CF>
where
    P: 'static + Send + Sync + TopKProvider<Score>,
    CF: ConditionForSegment<Score> + Sync,
{
    type Fruit = CollectionResult<Score>;
    type Child = TopSegmentCollector<Score, P::Child, CF::Type>;

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(&self, children: Vec<Self::Fruit>) -> Result<Self::Fruit> {
        Ok(P::merge_many(self.limit, children))
    }

    fn for_segment(
        &self,
        segment_id: SegmentLocalId,
        reader: &SegmentReader,
    ) -> Result<Self::Child> {
        Ok(TopSegmentCollector::new(
            segment_id,
            P::new_topk(self.limit),
            self.condition_factory.for_segment(reader),
        ))
    }
}

pub struct TopSegmentCollector<T, K, C> {
    total: usize,
    visited: usize,
    segment_id: SegmentLocalId,
    topk: K,
    condition: C,
    _marker: PhantomData<T>,
}

impl<T, K, C> TopSegmentCollector<T, K, C>
where
    K: TopK<T, DocId>,
    C: CheckCondition<T>,
{
    fn new(segment_id: SegmentLocalId, topk: K, condition: C) -> Self {
        Self {
            total: 0,
            visited: 0,
            segment_id,
            topk,
            condition,
            _marker: PhantomData,
        }
    }
}

impl<K, C> SegmentCollector for TopSegmentCollector<Score, K, C>
where
    K: TopK<Score, DocId> + 'static,
    C: CheckCondition<Score>,
{
    type Fruit = CollectionResult<Score>;

    fn collect(&mut self, doc: DocId, score: Score) {
        self.total += 1;
        if self.condition.check(self.segment_id, doc, score) {
            self.visited += 1;
            self.topk.visit(score, doc);
        }
    }

    fn harvest(self) -> Self::Fruit {
        let segment_id = self.segment_id;
        let items = self
            .topk
            .into_vec()
            .into_iter()
            .map(|(score, doc)| (score, DocAddress(segment_id, doc)))
            .collect();

        // XXX This is unsorted. It's ok because we sort during
        // merge, but using the same time to mean two things is
        // rather confusing
        CollectionResult {
            total: self.total,
            visited: self.visited,
            items,
        }
    }
}

#[derive(Debug)]
pub struct CollectionResult<T> {
    pub total: usize,
    pub visited: usize,
    pub items: Vec<(T, DocAddress)>,
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::conditional_collector::{
        topk::{AscendingTopK, DescendingTopK},
        Ascending, Descending,
    };

    use tantivy::{query::TermQuery, schema, Document, Index, Result, Term};

    #[test]
    fn condition_is_checked() {
        const LIMIT: usize = 4;

        let mut nil_collector = TopSegmentCollector::new(0, AscendingTopK::new(LIMIT), false);

        let mut top_collector = TopSegmentCollector::new(0, AscendingTopK::new(LIMIT), true);

        let condition = |_sid, doc, _score| doc % 2 == 1;

        let mut just_odds = TopSegmentCollector::new(0, AscendingTopK::new(LIMIT), condition);

        for i in 0..4 {
            nil_collector.collect(i, 420.0);
            top_collector.collect(i, 420.0);
            just_odds.collect(i, 420.0);
        }

        assert_eq!(0, nil_collector.harvest().items.len());
        assert_eq!(4, top_collector.harvest().items.len());

        // Verify that the collected items respect the condition
        let result = just_odds.harvest();
        assert_eq!(4, result.total);
        assert_eq!(2, result.items.len());
        for (score, doc) in result.items {
            let DocAddress(seg_id, doc_id) = doc;
            assert!(condition(seg_id, doc_id, score))
        }
    }

    #[test]
    fn collection_with_a_marker_smoke() {
        // Doc id=4 on segment=0 had score=0.5
        let marker = (0.5, DocAddress(0, 4));
        let mut collector = TopSegmentCollector::new(0, DescendingTopK::new(3), marker);

        // Every doc with a higher score has appeared already
        collector.collect(7, 0.6);
        collector.collect(5, 0.7);
        // assert_eq!(0, collector.len());

        // Docs with the same score, but lower id too
        collector.collect(3, 0.5);
        collector.collect(2, 0.5);
        // assert_eq!(0, collector.len());

        // And, of course, the same doc should not be collected
        collector.collect(4, 0.5);
        // assert_eq!(0, collector.len());

        // Lower scores are in
        collector.collect(1, 0.0);
        // Same score but higher doc, too
        collector.collect(6, 0.5);

        assert_eq!(2, collector.harvest().items.len());
    }

    #[test]
    fn collection_ordering_integration() -> Result<()> {
        let mut builder = schema::SchemaBuilder::new();

        let text_field = builder.add_text_field("text", schema::TEXT);

        let index = Index::create_in_ram(builder.build());
        let mut writer = index.writer_with_num_threads(1, 3_000_000)?;

        let add_doc = |text: &str| {
            let mut doc = Document::new();
            doc.add_text(text_field, text);
            writer.add_document(doc);
        };

        const NUM_DOCS: usize = 3;
        add_doc("the first doc is simple");
        add_doc("the second doc is a bit larger");
        add_doc("and the third document is rubbish");

        writer.commit()?;

        let reader = index.reader()?;
        let searcher = reader.searcher();

        let collector_asc = TopCollector::<_, Ascending, _>::new(NUM_DOCS, true);
        let collector_desc = TopCollector::<_, Descending, _>::new(NUM_DOCS, true);

        // Query for "the", which matches all docs and yields
        // a distinct score for each
        let query = TermQuery::new(
            Term::from_field_text(text_field, "the"),
            schema::IndexRecordOption::WithFreqsAndPositions,
        );
        let (asc, desc) = searcher.search(&query, &(collector_asc, collector_desc))?;

        assert_eq!(NUM_DOCS, asc.items.len());
        assert_eq!(NUM_DOCS, desc.items.len());

        let asc_scores = asc
            .items
            .iter()
            .map(|(score, _doc)| score)
            .collect::<Vec<_>>();

        let mut prev = None;
        for score in &asc_scores {
            if let Some(previous) = prev {
                assert!(previous < score, "The scores should be ascending");
            }
            prev = Some(score)
        }

        let mut desc_scores = desc
            .items
            .iter()
            .map(|(score, _doc)| score)
            .collect::<Vec<_>>();

        desc_scores.reverse();
        assert_eq!(asc_scores, desc_scores);

        Ok(())
    }
}
