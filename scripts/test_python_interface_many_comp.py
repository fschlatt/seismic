import json
from seismic import PySeismicIndexLargeVocabulary
import struct
from tqdm import tqdm

def write_sparse_vectors_to_binary_file(filename, term_id):
    # A binary sequence is a sequence of integers prefixed by its length, 
    # where both the sequence integers and the length are written as 32-bit little-endian unsigned integers.
    # Followed by a sequence of f32, with the same length
    def write_binary_sequence(lst_pairs, file): 
        file.write((len(lst_pairs)).to_bytes(4, byteorder='little', signed=False))   
        for v in lst_pairs:
            file.write((int(v[0])).to_bytes(4, byteorder='little', signed=False))
        for v in lst_pairs:
            value = v[1]
            ba = bytearray(struct.pack("f", value))  
            file.write(ba) 
    with open(filename, "wb") as fout:
        fout.write((len(term_id)).to_bytes(4, byteorder='little', signed=False))
        for d in tqdm(term_id):
            lst = sorted(list(d.items()))
            write_binary_sequence(lst, fout)


documents = [(91465, 1),(72621, 1), (65585, 1),(73298, 1), (64233, 1), (78115, 1), (63255, 1), (76142, 1)]

converted_docs = []

for i, (c, v) in enumerate(documents):
    converted_docs.append({c:v})
#print(converted_docs)


write_sparse_vectors_to_binary_file("toy_documents.bin", converted_docs)
write_sparse_vectors_to_binary_file("toy_query.bin", converted_docs[0:1])


# with open('toy_documents.json', 'w') as fp:
#     for doc in converted_docs:
#         fp.write(json.dumps(doc) + "\n")
    
# with open('toy_query.json', 'w') as fp:
#     json.dump(converted_docs[0], fp)
    
    
# with open('prova.json', "r") as f:
#     json_list = list(f)
    


index = PySeismicIndexLargeVocabulary.build("toy_documents.bin", 1, 1.0, False, 0, 0, 0.1)
k=1
query_cut=1
heap_factor=0.5
num_threads=1

### The index should return (1,0) (1 is the score, 0 is the index of the best document)
print(index.batch_search("toy_query.bin", k, query_cut, heap_factor, num_threads))